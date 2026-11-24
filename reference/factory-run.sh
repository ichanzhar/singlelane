#!/usr/bin/env bash
# One timer tick (every 15 min during working hours, see agent-factory@.timer).
# Checks the queue; if there is work, processes items back-to-back (DRAIN=true) until the
# queue is empty or working hours end. A job already running at WORK_END_HOUR finishes normally.
set -uo pipefail

SLOT=${1:?slot number required}
# shellcheck source=/dev/null
source "${CONF:-/etc/agent-factory/factory.env}"
START=${WORK_START_HOUR:-7}
END=${WORK_END_HOUR:-23}
DRAIN=${DRAIN:-true}

in_hours() {
  local h=$((10#$(date +%H)))
  (( h >= START && h < END ))
}

while in_hours; do
  /opt/agent-factory/bin/factory.sh "$SLOT"
  rc=$?
  case $rc in
    10) exit 0 ;;                       # queue empty or paused - wait for next tick
    0)  [[ $DRAIN == true ]] || exit 0 ;; # item done - take the next one right away
    *)  exit "$rc" ;;                   # failure - stop, do not burn through the queue
  esac
done
exit 0
