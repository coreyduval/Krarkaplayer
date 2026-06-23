#!/usr/bin/env bash
# Overnight watchdog: forces shutdown if no progress for STALL seconds, or at the 6h hard cap.
# Normal/graceful path sets overnight_DONE.flag first (watchdog then exits without shutting down).
PROG="/c/KrarkaplayerClaude/overnight/progress.log"
DONE="/c/KrarkaplayerClaude/overnight/overnight_DONE.flag"
START=$(date +%s)
DEADLINE=$((START + 6*3600))   # 6h hard cap
STALL=2700                     # 45 min with no new progress -> shutdown
last_size=0
last_change=$START
shutdown_now() {
  echo "$(date '+%Y-%m-%d %H:%M:%S') WATCHDOG: $1 -> SHUTDOWN" >> "$PROG"
  powershell.exe -NoProfile -Command "Stop-Computer -Force"
  exit 0
}
while true; do
  sleep 120
  now=$(date +%s)
  [ -f "$DONE" ] && { echo "$(date '+%F %T') WATCHDOG: DONE flag seen, exiting (graceful path owns shutdown)" >> "$PROG"; exit 0; }
  [ "$now" -ge "$DEADLINE" ] && shutdown_now "6h hard cap reached"
  sz=$(wc -c < "$PROG" 2>/dev/null || echo 0)
  if [ "$sz" -gt "$last_size" ]; then
    last_size=$sz; last_change=$now
  elif [ $((now - last_change)) -ge "$STALL" ]; then
    shutdown_now "no progress for ${STALL}s (stall)"
  fi
done
