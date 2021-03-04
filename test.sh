#!/bin/sh

dbus-run-session --config-file=tests/session.conf -- sh -c 'echo $DBUS_SESSION_BUS_ADDRESS
dbus-test-tool echo --sleep-ms=1000 --name=io.maves.LongWait &
dbus-test-tool echo --sleep-ms=50 --name=io.maves.ShortWait & 
dbus-test-tool echo --sleep-ms=0 --name=io.maves.NoWait & 
cargo test && cargo test -- --ignored'