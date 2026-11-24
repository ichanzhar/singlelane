//! Logging setup. Every log line passes through the redactor before it leaves the process
//! (G8, SPEC 12).
//!
//! CLI commands log human-readable lines to stderr so they don't mix with command output.
//! The daemon (phase 5) switches to JSON on stdout for journald.

use std::io::Write;
use std::sync::Arc;

use singlelane_core::policy::redact::Redactor;
use tracing_subscriber::EnvFilter;
use tracing_subscriber::fmt::MakeWriter;

/// Where redacted log bytes go.
type Sink = Arc<dyn Fn() -> Box<dyn Write + Send> + Send + Sync>;

/// A [`MakeWriter`] that buffers each log event and writes it redacted.
#[derive(Clone)]
pub struct RedactingMakeWriter {
    redactor: Arc<Redactor>,
    sink: Sink,
}

impl std::fmt::Debug for RedactingMakeWriter {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RedactingMakeWriter")
            .finish_non_exhaustive()
    }
}

impl RedactingMakeWriter {
    /// Redacted writes to `sink`.
    pub fn new(redactor: Redactor, sink: Sink) -> Self {
        Self {
            redactor: Arc::new(redactor),
            sink,
        }
    }

    /// Redacted writes to stderr.
    pub fn stderr(redactor: Redactor) -> Self {
        Self::new(redactor, Arc::new(|| Box::new(std::io::stderr())))
    }
}

/// Buffers one event; redacts and writes it on drop (the formatter writes an event in pieces,
/// and a secret may span two pieces).
pub struct RedactingWriter {
    buf: Vec<u8>,
    redactor: Arc<Redactor>,
    sink: Sink,
}

impl Write for RedactingWriter {
    fn write(&mut self, data: &[u8]) -> std::io::Result<usize> {
        self.buf.extend_from_slice(data);
        Ok(data.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl Drop for RedactingWriter {
    fn drop(&mut self) {
        if self.buf.is_empty() {
            return;
        }
        let text = String::from_utf8_lossy(&self.buf);
        let redacted = self.redactor.redact(&text);
        let mut out = (self.sink)();
        // Nowhere to report a failed log write.
        let _ = out.write_all(redacted.as_bytes());
        let _ = out.flush();
    }
}

impl<'a> MakeWriter<'a> for RedactingMakeWriter {
    type Writer = RedactingWriter;

    fn make_writer(&'a self) -> Self::Writer {
        RedactingWriter {
            buf: Vec::new(),
            redactor: Arc::clone(&self.redactor),
            sink: Arc::clone(&self.sink),
        }
    }
}

/// Text logs to stderr, level from `RUST_LOG` (default `warn`).
pub fn init(redactor: Redactor) {
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("warn"));
    // Ignore the error if a subscriber is already set (tests).
    let _ = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(RedactingMakeWriter::stderr(redactor))
        .with_target(false)
        .try_init();
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use super::*;

    #[derive(Clone, Default)]
    struct Shared(Arc<Mutex<Vec<u8>>>);

    impl Write for Shared {
        fn write(&mut self, data: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(data);
            Ok(data.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn g8_log_lines_are_redacted() {
        let shared = Shared::default();
        let sink_target = shared.clone();
        let writer = RedactingMakeWriter::new(
            Redactor::new(),
            Arc::new(move || Box::new(sink_target.clone())),
        );
        let subscriber = tracing_subscriber::fmt()
            .with_writer(writer)
            .with_ansi(false)
            .finish();
        let token = format!("ghs_{}", "Lg8Tk9Pq0R".repeat(4));
        tracing::subscriber::with_default(subscriber, || {
            tracing::warn!(detail = %format!("git failed with {token}"), "push failed");
        });
        let out = String::from_utf8(shared.0.lock().unwrap().clone()).unwrap();
        assert!(out.contains("push failed"), "{out}");
        assert!(!out.contains(&token), "{out}");
        assert!(out.contains("[REDACTED]"));
    }

    #[test]
    fn g8_secret_split_across_writes_is_still_redacted() {
        let shared = Shared::default();
        let sink_target = shared.clone();
        let writer = RedactingMakeWriter::new(
            Redactor::new(),
            Arc::new(move || Box::new(sink_target.clone())),
        );
        let token = format!("ghs_{}", "Sp1itT0k3n".repeat(4));
        {
            let mut w = writer.make_writer();
            let (a, b) = token.split_at(10);
            w.write_all(a.as_bytes()).unwrap();
            w.write_all(b.as_bytes()).unwrap();
        }
        let out = String::from_utf8(shared.0.lock().unwrap().clone()).unwrap();
        assert!(!out.contains(&token));
    }
}
