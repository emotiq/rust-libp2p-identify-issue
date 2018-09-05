# rust-libp2p-identify-issue

Small example to demonstrate issues with `rust-libp2p` Identify protocol.

How to run:

```shell
cargo build
cargo run -p echo-server
```

from another terminal window run from the same folder:

```shell
cargo run -p echo-dialer
```

Under normal circumstances `echo-dialer` should send and receive `"hello world"` message.
In reality it blocks in transport upgrade and connection is never reaches `echo` protocol.