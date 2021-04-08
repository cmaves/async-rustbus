#!/bin/sh

if ! [ -e /var/run/dbus/system_bus_socket ]; then
    echo Dbus Daemon not found, starting...
    mkdir -p /var/run/dbus
    dbus-daemon --system --fork
fi
dbus-run-session --config-file=tests/session.conf -- sh -c 'echo $DBUS_SESSION_BUS_ADDRESS
dbus-test-tool echo --sleep-ms=1000 --name=io.maves.LongWait &
dbus-test-tool echo --sleep-ms=50 --name=io.maves.ShortWait & 
dbus-test-tool echo --sleep-ms=0 --name=io.maves.NoWait & 
cargo fmt -- --check && cargo clippy --all-targets &&
cargo test && cargo test -- --ignored'
