# corvo-chat-client

The clients for corvo-chat, a 1:1 end-to-end encrypted chat that runs through
its own server ([corvo-chat-server](https://github.com/Abinsanth/corvo-chat-server)).

- `crates/corvo-chat-core`: the shared core (crypto, server connection, sync).
  It builds for the browser too, so web and phone clients can reuse it.
- `crates/corvo-chat`: the terminal app. For now it's the empty shell: the
  tabs, the input, `/help` and `/about`. Signing in and chatting come next.

## Build

```sh
cargo build
```

Run the terminal app:

```sh
cargo run -p corvo-chat
```

It keeps its files in `~/.config/corvo-chat` (`$XDG_CONFIG_HOME/corvo-chat`
if that's set, `%APPDATA%\corvo-chat` on Windows). Set `CORVO_CHAT_LOG=debug`
for more in its log, `corvo-chat.log` there.

Check that the core still builds for the browser:

```sh
rustup target add wasm32-unknown-unknown
cargo build -p corvo-chat-core --target wasm32-unknown-unknown --no-default-features
```

## License

MIT
