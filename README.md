# async-rustbus
This crate implements an async API for interacting with DBus. 
It is built on top of KillingSpark's existing [rustbus](https://github.com/KillingSpark/rustbus) crate 
and reuses its implementation of the DBus wire format and message protocol.
A non-blocking reimplementation of rustbus's `Conn` is used to build an async version of [`RpcConn`](https://docs.rs/rustbus/0.12.0/rustbus/connection/rpc_conn/struct.RpcConn.html).
This crate's async version of RpcConn creates an async API for connecting to system, session and remote (TCP) DBus daemons, sending messages, receving method calls and signals.
## Design differences between this crate and `rustbus` and why
While the async crate reuses the message format of the parent crate, there were some design decisions that differ from the parent other.

* Most `RpcConn` functions in this crate return `std::io::Result`s rather than a custom error type. 
When sending/receiving messages the errors that can occur are IO errors, (un)marshalling errors and invalid headers.
Because most user's are typically interacting with well tested DBus implementations, unmarshalling errors are likely caused by bugs in our implementation.
Marshalling errors are typically caught by the remote dbus-daemon, causing a disconnect, which results in a future `std::io::Error`.
Invalid headers and invalid Fds are also possible sources of errors but typically bugs on the user's end that should be solved; 
they do not need to handled at runtime.
This really leaves only IO errors that needed to be handled at runtime. Other errors are propagated custom `std::io::Error`s;

* This crate terminates connections when protocol violations occur (the original allows for attempting to recover). 
When protocol violations occur, there is no safe and consistent way to recover. While keeping the connection open can be helpful for debugging
it is generally useless for the user of the crate. 
Also the [DBus Specification](https://dbus.freedesktop.org/doc/dbus-specification.html#message-protocol-handling-invalid) specifies that no recovery should be atttempted:
>> For security reasons, the D-Bus protocol should be strictly parsed and validated, with the exception of defined extension points. 
>> Any invalid protocol or spec violations should result in immediately dropping the connection without notice to the other end. 

* The original crate requires the user manually wait for responses. 
If the responses are never retrieved after being received back, they will create a small memory leak.
When creating method calls in this crate, this crate provides a Future, that is used to retrieve the response. 
In the event that this Future is dropped then response will be ignored and cleaned up apporiately.

* This crate's `RpcConn` sends and waits for the DBus hello message on connection.
The original requires that you manually handle this message and response.

* This crate support TCP streams for connection to remote servers. 
*Note:* TCP streams are unencrypted and should be done using an SSH-tunnel or something else.

* The original crate allows for finer control of the how information is written to and received from the socket, 
with ability to set timeouts and choice when buffers are refilled.
Async functionality generally elimnates any necessity for this by abstracting over non-blocking system calls. 

## Status
This has yet to released as a crate. It currently relies of some seperate patches to `rustbus` that have yet to be pulled, making it impossible to publish.
Why some basic testing is working, it is far from being stress tested.
