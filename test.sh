#!/bin/sh

if ! [ -e /var/run/dbus/system_bus_socket ]; then
    echo Dbus Daemon not found, starting...
    mkdir -p /var/run/dbus
    dbus-daemon --system --fork
fi
dbus-run-session --config-file=tests/session.conf -- sh -c 'echo $DBUS_SESSION_BUS_ADDRESS
dbus-test-tool echo --sleep-ms=1000 --name=io.test.LongWait &
dbus-test-tool echo --sleep-ms=20 --name=io.test.ShortWait &
dbus-test-tool echo --sleep-ms=0 --name=io.test.NoWait &
cargo test --lib &&
cargo test --test stress --test rpc_conn &&
cargo test -- --ignored --test-threads 1 &&
cargo test --doc'
