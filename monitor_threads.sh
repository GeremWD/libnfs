#!/bin/bash
# Launch a command in the background and report its peak thread count.

"$@" &
PID=$!

PEAK=0
while kill -0 "$PID" 2>/dev/null; do
    COUNT=$(ls "/proc/$PID/task" 2>/dev/null | wc -l)
    if [ "$COUNT" -gt "$PEAK" ]; then
        PEAK=$COUNT
    fi
    sleep 0.001
done

wait "$PID"
EXIT=$?

echo "----"
echo "Peak thread count: $PEAK"
echo "Exit code: $EXIT"
