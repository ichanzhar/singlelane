//! Runs a child in its own process group, with a deadline, and pumps its output line by line.
//!
//! SPEC 7: Claude Code runs in its own process group; when `JOB_TIMEOUT` expires the whole
//! group gets SIGTERM, and SIGKILL 60 s later. The same happens when agent-runner itself is
//! told to stop (`docker stop`, `docker compose down`).

use std::io::{self, BufRead, BufReader, Read};
use std::os::unix::process::{CommandExt, ExitStatusExt};
use std::process::{Child, Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::thread;
use std::time::{Duration, Instant};

use rustix::process::{Pid, Signal, kill_process_group};

/// How often the child is checked.
const POLL: Duration = Duration::from_millis(50);
/// How long to wait for the output pipes to reach end-of-file after the child has exited.
/// They normally close at once; a process outside the group that inherited them (a detached
/// dev server) can keep them open forever.
const DRAIN_WAIT: Duration = Duration::from_secs(5);
/// Longest chunk handed to a sink in one piece. Claude Code's lines are far shorter; the cap
/// only bounds memory if something prints without ever ending a line.
const MAX_LINE: u64 = 32 * 1024 * 1024;

/// Receives output: one call per line, including its line ending (the last line of a stream
/// may come without one, an over-long line comes in pieces).
pub type LineSink = Box<dyn FnMut(&[u8]) + Send + 'static>;

/// Set when agent-runner is asked to stop.
#[derive(Debug, Clone, Default)]
pub struct Shutdown(Arc<AtomicBool>);

impl Shutdown {
    /// A flag that nothing sets by itself (tests, `--smoke-test`).
    pub fn new() -> Self {
        Self::default()
    }

    /// A flag set by SIGTERM and SIGINT. agent-runner is PID 1 in its container, where signals
    /// without a handler are ignored, so without this `docker stop` would wait out its grace
    /// period and then kill everything unreported.
    pub fn install() -> io::Result<Self> {
        let shutdown = Self::new();
        for signal in [signal_hook::consts::SIGTERM, signal_hook::consts::SIGINT] {
            signal_hook::flag::register(signal, Arc::clone(&shutdown.0))?;
        }
        Ok(shutdown)
    }

    /// Asks for a stop.
    pub fn request(&self) {
        self.0.store(true, Ordering::SeqCst);
    }

    /// Whether a stop was asked for.
    pub fn requested(&self) -> bool {
        self.0.load(Ordering::SeqCst)
    }
}

/// How long the child may run.
#[derive(Debug, Clone, Copy)]
pub struct Limits {
    /// SIGTERM goes to the process group after this long.
    pub timeout: Duration,
    /// SIGKILL follows this long after SIGTERM.
    pub kill_grace: Duration,
}

/// How the child ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChildExit {
    /// The exit code, if the child exited by itself.
    pub exit_code: Option<i32>,
    /// The signal that ended the child, if one did.
    pub signal: Option<i32>,
    /// The deadline passed and the group was signalled.
    pub timed_out: bool,
    /// A [`Shutdown`] was requested and the group was signalled.
    pub terminated: bool,
    /// The child outlived the grace period and got SIGKILL.
    pub killed: bool,
    /// How long the child ran.
    pub wall: Duration,
    /// Whether both output pipes reached end-of-file before [`DRAIN_WAIT`] ran out.
    pub output_complete: bool,
}

/// Runs `command` to completion in its own process group. Stdin is `/dev/null`: Claude Code
/// otherwise waits a few seconds for piped input before it starts.
pub fn run_in_group(
    command: &mut Command,
    limits: Limits,
    shutdown: &Shutdown,
    on_stdout: LineSink,
    on_stderr: LineSink,
) -> io::Result<ChildExit> {
    command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .process_group(0);
    let start = Instant::now();
    let mut child = command.spawn()?;
    let group = Pid::from_child(&child);

    let (done_tx, done_rx) = mpsc::channel();
    let mut pumps = 0;
    if let Some(stdout) = child.stdout.take() {
        pump(stdout, on_stdout, done_tx.clone());
        pumps += 1;
    }
    if let Some(stderr) = child.stderr.take() {
        pump(stderr, on_stderr, done_tx.clone());
        pumps += 1;
    }
    drop(done_tx);

    let deadline = start.checked_add(limits.timeout);
    let mut signalled_at: Option<Instant> = None;
    let mut timed_out = false;
    let mut terminated = false;
    let mut killed = false;
    let status = loop {
        if let Some(status) = child.try_wait()? {
            break status;
        }
        let now = Instant::now();
        match signalled_at {
            None => {
                if shutdown.requested() {
                    terminated = true;
                } else if deadline.is_some_and(|d| now >= d) {
                    timed_out = true;
                }
                if terminated || timed_out {
                    signal_group(group, Signal::TERM);
                    signalled_at = Some(now);
                }
            }
            Some(at) if !killed && now.duration_since(at) >= limits.kill_grace => {
                signal_group(group, Signal::KILL);
                killed = true;
            }
            Some(_) => {}
        }
        thread::sleep(POLL);
    };
    let wall = start.elapsed();

    // The leader is gone and the job is over: nothing in its group may live on or keep the
    // output pipes open. What is already in the pipes stays readable.
    signal_group(group, Signal::KILL);
    let output_complete = wait_for_pumps(&done_rx, pumps, DRAIN_WAIT);

    Ok(ChildExit {
        exit_code: status.code(),
        signal: status.signal(),
        timed_out,
        terminated,
        killed,
        wall,
        output_complete,
    })
}

/// Ends a helper process (and its group) that is no longer needed.
pub fn kill_group_of(child: &mut Child) {
    signal_group(Pid::from_child(child), Signal::KILL);
    // The leader was our child: reap it. Errors mean it is already gone.
    let _ = child.kill();
    let _ = child.wait();
}

/// Signals a process group. Errors are ignored on purpose: the only expected one is "no such
/// process", when the group has already ended.
fn signal_group(group: Pid, signal: Signal) {
    let _ = kill_process_group(group, signal);
}

fn pump(reader: impl Read + Send + 'static, mut sink: LineSink, done: Sender<()>) {
    thread::spawn(move || {
        let mut reader = BufReader::with_capacity(64 * 1024, reader);
        let mut line = Vec::new();
        loop {
            line.clear();
            match (&mut reader).take(MAX_LINE).read_until(b'\n', &mut line) {
                Ok(0) => break,
                Ok(_) => sink(&line),
                Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                Err(_) => break,
            }
        }
        drop(sink);
        // The receiver is gone if the wait for this pump was given up.
        let _ = done.send(());
    });
}

fn wait_for_pumps(done: &Receiver<()>, pumps: usize, wait: Duration) -> bool {
    let deadline = Instant::now() + wait;
    for _ in 0..pumps {
        let left = deadline.saturating_duration_since(Instant::now());
        if done.recv_timeout(left).is_err() {
            return false;
        }
    }
    true
}
