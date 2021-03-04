#!/bin/sh

dbus-run-session --config-file=tests/session.conf -- sh -c 'echo $DBUS_SESSION_BUS_ADDRESS
dbus-test-tool echo --sleep-ms=1000 --name=io.maves.LongWait &
dbus-test-tool echo --sleep-ms=50 --name=io.maves.ShortWait & 
cargo test -- --ignored --nocapture no_recv_deadlock'
