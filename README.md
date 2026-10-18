# corvo-chat-client

The clients for corvo-chat, a 1:1 end-to-end encrypted chat that runs through
its own server ([corvo-chat-server](https://github.com/Abinsanth/corvo-chat-server)).

- `crates/corvo-chat-core`: the shared core (crypto, server connection, sync).
  It builds for the browser too, so web and phone clients can reuse it.
- The terminal app comes next, as a second crate in this workspace.

## Build

```sh
cargo build
```

Check that the core still builds for the browser:

```sh
rustup target add wasm32-unknown-unknown
cargo build -p corvo-chat-core --target wasm32-unknown-unknown --no-default-features
```

## License

MIT
