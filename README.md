# corvo-chat-client

The clients for corvo-chat, a 1:1 end-to-end encrypted chat that runs through
its own server ([corvo-chat-server](https://github.com/Abinsanth/corvo-chat-server)).

- `crates/corvo-chat-core`: the shared core (crypto, server connection, sync).
  It builds for the browser too, so web and phone clients can reuse it.
- `crates/corvo-chat`: the terminal app: signing up (with an
  invite), signing in, recovery with the recovery code, unlocking, and
  chatting: the sidebar of contacts and requests, a tab per chat
  (`/chat <name|number>`, `/add <number>`, `/code` for your own number),
  marks (`○ · ✓ ✓✓ ✗`), and older messages as you scroll up. Messages
  typed offline wait and go once the server is back. The people commands:
  `/invite`, `/accept`, `/reject`, `/block`, `/unblock`, `/remove`,
  `/report`, `/contacts`, `/requests`, `/blocked`, `/info`, and `/verify`
  to compare safety codes. If someone's keys change, the app says so and
  sends them nothing. `/profile` opens the Profile tab: your display
  name, number and username, Change password, New recovery code, your
  devices (sign another one out, rename this one) and Delete account.
  A device signed out from elsewhere goes back to Sign in / Sign up and
  deletes its key file. Forgot your password? Press Esc at the password
  prompt to sign this computer out, then sign in again or use your
  recovery code.

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
for more in its log, `corvo-chat.log` there. `key.json` holds your account's
keys, locked with your password and readable only by you.

Check that the core still builds for the browser:

```sh
rustup target add wasm32-unknown-unknown
cargo build -p corvo-chat-core --target wasm32-unknown-unknown --no-default-features
```

## License

MIT
