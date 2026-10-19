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

## Install

On macOS or Linux (x86_64 or ARM):

```sh
curl -fsSL https://raw.githubusercontent.com/Abinsanth/corvo-chat-client/release/install.sh | sh
```

On Windows 10 or 11 (x86_64), in PowerShell:

```powershell
irm https://raw.githubusercontent.com/Abinsanth/corvo-chat-client/release/install.ps1 | iex
```

The installer downloads the build for your system, checks its SHA-256 and
installs it without sudo or admin rights. On macOS and Linux it goes to
`~/.local/bin`, which it adds to your PATH in `~/.zshrc` (zsh, in `$ZDOTDIR`
if you set one) or `~/.bash_profile` / `~/.bashrc` (bash), once, marked with
a comment. For fish and other shells it prints what to run instead. On
Windows it goes to `%LOCALAPPDATA%\Programs\corvo-chat`, added to your user
PATH. Set `CORVO_CHAT_NO_MODIFY_PATH=1` to leave your profile (or PATH)
alone, `CORVO_CHAT_INSTALL_DIR` to install somewhere else, and
`CORVO_CHAT_VERSION` to pick a release, like `v0.1.0` (leave it out for the
latest).

## Update

```sh
corvo-chat --update
```

The app tells you when a newer version is out.

## Uninstall

```sh
corvo-chat --uninstall
```

It asks for your password to sign this computer out (press Enter to skip),
then removes the data folder, the PATH line the installer added and the app
itself. Your account stays: install again and sign in to get your chats back.

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

## Releasing

For the maintainer: bump the version in `Cargo.toml`, merge it to `main`, then
merge `main` into the `release` branch. The Release workflow builds the five
platforms and publishes `v<version>` (a version with a `-` is a pre-release).
It refuses a version that is already released.

## License

MIT
