//! Carries out [`Effect`]s: the core, the files and the terminal. What came
//! of each goes back to `update` as [`Msg::Done`].
//!
//! The start screens' effects run the account flows on a connection of their
//! own, save the key file, then start the core's driver, which the runner
//! keeps from then on. Calls to it run on their own, so the screen stays
//! live while the server answers; their replies and the core's updates come
//! out of [`Runner::next`].

use std::collections::VecDeque;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use corvo_chat_core::native::{Command, Driver, NativeTransport, Reply, Roots, first_start};
use corvo_chat_core::{
    ConnState, Error, LocalKey, Recovery, SignedIn, SignedUp, Strength, Update, hello, sign_in, sign_up,
};
use corvo_chat_proto::B64;
use corvo_chat_proto::frames::{Account, Device, DeviceKind, SignedOutReason};
use ratatui::crossterm::execute;
use ratatui::crossterm::terminal::SetTitle;
use tokio::sync::mpsc::{self, UnboundedReceiver, UnboundedSender};

use super::start::{say, signed_out_note};
use super::state::{Secret, VERSION};
use super::update::{Effect, Msg, Outcome};
use crate::files::{CACHE_FILE, KEY_FILE, USERNAME_FILE, device_id, device_name, username, write_private};

/// The public server, where accounts are made and signed in to.
pub const DEFAULT_SERVER: &str = "wss://chat.corvo.live/ws";

/// A server to sign up and sign in at, and the certificates it may present.
/// Tests pass one running in-process.
#[derive(Debug, Clone)]
pub struct Server {
    pub url: String,
    pub roots: Roots,
}

impl Default for Server {
    /// [`DEFAULT_SERVER`], with the usual public authorities.
    fn default() -> Server {
        Server { url: DEFAULT_SERVER.into(), roots: Roots::Web }
    }
}

/// What the loop does after an effect.
#[derive(Debug)]
pub enum Flow {
    /// Carry on, feeding this to `update` if there is one.
    Next(Option<Msg>),
    Quit,
    /// Leave, printing this.
    Exit(String),
    /// The account is deleted: leave, printing the goodbye and these.
    Leave(Vec<String>),
}

/// Said when the first connection of a key file never made it.
const NO_SERVER: &str = "Couldn't reach the server. Check your connection and try again.";
/// Said when the server needs a newer app.
const TOO_OLD: &str = "This version is too old. Update corvo-chat.";

/// How long stopping waits for the core to close its files.
const STOP_WAIT: std::time::Duration = std::time::Duration::from_secs(3);

/// Why the account couldn't be read at unlock.
enum Stop {
    Failed(String),
    /// This device was signed out, before this run or while unlocking.
    SignedOut(SignedOutReason),
}

pub struct Runner<W: Write> {
    /// The data folder.
    dir: PathBuf,
    server: Server,
    strength: Strength,
    /// The terminal: window titles are written here.
    out: W,
    /// The core, once signed in or unlocked.
    driver: Option<Driver>,
    updates: Option<UnboundedReceiver<Update>>,
    /// Updates read while starting, still to hand out.
    held: VecDeque<Update>,
    /// Where calls to the core send their replies, and where they're read.
    reply_to: UnboundedSender<Outcome>,
    replies: UnboundedReceiver<Outcome>,
    /// Calls sent whose replies haven't been read yet.
    calls: usize,
    /// Connections opened, for tests.
    #[cfg(test)]
    connections: usize,
}

impl<W: Write> Runner<W> {
    /// `dir` is the data folder; `server` is where to sign up and sign in.
    pub fn new(dir: PathBuf, server: Server, strength: Strength, out: W) -> Runner<W> {
        let (reply_to, replies) = mpsc::unbounded_channel();
        Runner {
            reply_to,
            replies,
            calls: 0,
            dir,
            server,
            strength,
            out,
            driver: None,
            updates: None,
            held: VecDeque::new(),
            #[cfg(test)]
            connections: 0,
        }
    }

    /// Does `effect`. An error here is the terminal failing; everything else
    /// comes back as an `Outcome`.
    pub async fn apply(&mut self, effect: Effect) -> anyhow::Result<Flow> {
        let outcome = match effect {
            Effect::Quit => return Ok(Flow::Quit),
            Effect::Exit(why) => return Ok(Flow::Exit(why)),
            Effect::SetTitle(title) => {
                execute!(self.out, SetTitle(title))?;
                return Ok(Flow::Next(None));
            }
            Effect::SignUp { base, name, password } => self.sign_up(&base, &name, &password).await,
            Effect::SignIn { username, password } => self.sign_in(&username, &password).await,
            Effect::Recover { username, code, password } => self.recover(&username, &code, &password).await,
            Effect::Unlock { password } => self.unlock(&password).await,
            Effect::Activate { code } => self.activate(&code).await,
            Effect::Call(asked) => {
                self.call(asked);
                return Ok(Flow::Next(None));
            }
            Effect::SignedOut(note) => Ok(self.signed_out(note).await),
            Effect::Leave(notes) => return Ok(self.leave(notes).await),
        };
        Ok(Flow::Next(Some(Msg::Done(outcome.unwrap_or_else(Outcome::Failed)))))
    }

    /// The next reply to a call, or the core's next update, whichever
    /// comes first. Waits forever while there's neither.
    pub async fn next(&mut self) -> Msg {
        tokio::select! {
            Some(outcome) = self.replies.recv() => {
                self.calls -= 1;
                Msg::Done(outcome)
            }
            update = next_update(&mut self.held, &mut self.updates) => Msg::Core(update),
        }
    }

    /// Stops the core.
    pub async fn shutdown(&mut self) {
        self.stop_and_wait().await;
    }

    fn stop(&mut self) {
        // Dropping the driver stops it.
        self.driver = None;
        self.updates = None;
        self.held.clear();
    }

    /// Stops the core and waits until it has, a few seconds at most, so its
    /// saved chats are closed: a file still open can't be deleted on Windows.
    async fn stop_and_wait(&mut self) {
        let updates = self.updates.take();
        self.stop();
        if let Some(mut updates) = updates {
            // The core drops its end once it has stopped.
            let stopped = async { while updates.recv().await.is_some() {} };
            let _ = tokio::time::timeout(STOP_WAIT, stopped).await;
        }
    }

    /// This device: its id (made on the first start) and the computer's name.
    fn device(&self) -> Result<Device, String> {
        let id = device_id(&self.dir).map_err(|e| format!("Couldn't save this device's id: {e}"))?;
        Ok(Device { id: B64(id), kind: DeviceKind::Cli, name: device_name() })
    }

    /// A connection to the server that has said hello.
    async fn connect(&mut self) -> Result<NativeTransport, String> {
        #[cfg(test)]
        {
            self.connections += 1;
        }
        let mut t =
            NativeTransport::connect(&self.server.url, self.server.roots.clone()).await.map_err(|e| say(&e.into()))?;
        hello(&mut t, VERSION).await.map_err(|e| say(&e))?;
        Ok(t)
    }

    async fn sign_up(&mut self, base: &str, name: &str, password: &Secret) -> Result<Outcome, String> {
        let device = self.device()?;
        let mut t = self.connect().await?;
        let up = sign_up(&mut t, base, name, password.expose(), &device, self.strength).await.map_err(|e| say(&e))?;
        // The driver resumes on a connection of its own, which would close this one.
        drop(t);
        self.signed_up(up, password, device)
    }

    async fn sign_in(&mut self, username: &str, password: &Secret) -> Result<Outcome, String> {
        let device = self.device()?;
        let mut t = self.connect().await?;
        let signed_in = sign_in(&mut t, username, password.expose(), &device, self.strength).await;
        drop(t);
        let signed_in = signed_in.map_err(|e| say(&e))?;
        self.begin(&signed_in, password, device)?;
        Ok(running(signed_in.account))
    }

    async fn recover(&mut self, username: &str, code: &Secret, password: &Secret) -> Result<Outcome, String> {
        let device = self.device()?;
        let mut t = self.connect().await?;
        let up = match Recovery::start(&mut t, username, code.expose(), &device).await {
            Ok(mut recovery) => recovery.complete(password.expose(), self.strength).await,
            Err(e) => Err(e),
        };
        drop(t);
        let up = up.map_err(|e| say(&e))?;
        self.signed_up(up, password, device)
    }

    /// Starts on an account just made or recovered, keeping its code to show.
    /// The account exists on the server now, so the code is shown even if
    /// starting here fails; that failure is said once the code is confirmed.
    fn signed_up(&mut self, up: SignedUp, password: &Secret, device: Device) -> Result<Outcome, String> {
        let failed = self.begin(&up.signed_in, password, device).err();
        let account = up.signed_in.account;
        let code = Secret::new(up.recovery_code.display());
        Ok(Outcome::SignedUp { username: account.username.clone(), code, account, failed })
    }

    /// Saves the key file and the username for an account just signed in,
    /// then starts the core on it.
    fn begin(&mut self, signed_in: &SignedIn, password: &Secret, device: Device) -> Result<(), String> {
        let key = first_start(&self.server.url, signed_in, password.expose(), &device, self.strength);
        // There was no key file, so any saved chats here belong to no one now.
        delete_cache(&self.dir.join(CACHE_FILE));
        write_private(&self.dir.join(KEY_FILE), key.to_json().as_bytes())
            .map_err(|e| format!("Couldn't save your key file: {e}"))?;
        write_private(&self.dir.join(USERNAME_FILE), signed_in.account.username.as_bytes())
            .map_err(|e| format!("Couldn't save your username: {e}"))?;
        self.run(key, password, device)
    }

    /// Starts the driver on `key`: it unlocks it, opens the saved chats and connects.
    fn run(&mut self, key: LocalKey, password: &Secret, device: Device) -> Result<(), String> {
        self.stop();
        let cache = self.dir.join(CACHE_FILE);
        let (driver, updates) =
            Driver::start(self.server.roots.clone(), key, password.expose(), cache, device, self.strength, VERSION)
                .map_err(|e| say(&e))?;
        self.driver = Some(driver);
        self.updates = Some(updates);
        Ok(())
    }

    /// Opens the key file with the password and starts the core. It runs on
    /// the server the key file names. A device signed out meanwhile goes
    /// back to the start, as if it happened while open.
    async fn unlock(&mut self, password: &Secret) -> Result<Outcome, String> {
        let json =
            fs::read_to_string(self.dir.join(KEY_FILE)).map_err(|e| format!("Couldn't read your key file: {e}"))?;
        let key = LocalKey::from_json(&json).map_err(|e| say(&e))?;
        let device = Device { id: B64(key.device_id), kind: DeviceKind::Cli, name: device_name() };
        self.run(key, password, device)?;
        match self.account().await {
            Ok(account) => Ok(running(account)),
            Err(Stop::SignedOut(reason)) => Ok(self.signed_out(signed_out_note(reason).into()).await),
            Err(Stop::Failed(why)) => {
                self.stop_and_wait().await;
                Err(why)
            }
        }
    }

    /// The account as the core saved it. A key file whose first connection
    /// never made it has none saved: then this waits for the first one.
    async fn account(&mut self) -> Result<Account, Stop> {
        let failed = |e: &Error| Stop::Failed(say(e));
        let mut waited = false;
        loop {
            let driver = self.driver.as_ref().ok_or_else(|| failed(&Error::Stopped))?;
            match driver.call(Command::Account).await {
                Ok(Reply::Account(account)) => return Ok(account),
                Ok(_) => return Err(failed(&Error::BadServerMessage)),
                Err(Error::Offline) if !waited => waited = true,
                Err(Error::SignedOut(reason)) => return Err(Stop::SignedOut(reason)),
                Err(Error::Stopped) => return Err(self.why_stopped().await),
                Err(e) => return Err(failed(&e)),
            }
            let updates = self.updates.as_mut().ok_or_else(|| failed(&Error::Stopped))?;
            loop {
                let Some(update) = updates.recv().await else { return Err(failed(&Error::Stopped)) };
                match update {
                    Update::Connection(ConnState::Online) => {
                        self.held.push_back(update);
                        break;
                    }
                    Update::Connection(ConnState::UpdateRequired) => return Err(Stop::Failed(TOO_OLD.into())),
                    Update::Connection(_) => return Err(Stop::Failed(NO_SERVER.into())),
                    Update::SignedOut(reason) => return Err(Stop::SignedOut(reason)),
                    other => self.held.push_back(other),
                }
            }
        }
    }

    /// Why the driver stopped while unlocking: a sign-out says so in its
    /// last updates. It has stopped, so reading them ends.
    async fn why_stopped(&mut self) -> Stop {
        if let Some(updates) = self.updates.as_mut() {
            while let Some(update) = updates.recv().await {
                if let Update::SignedOut(reason) = update {
                    return Stop::SignedOut(reason);
                }
            }
        }
        Stop::Failed(say(&Error::Stopped))
    }

    /// Signed out: stops the core, then deletes the key file and the saved
    /// chats. The device id and the username stay, for signing in again.
    async fn signed_out(&mut self, note: String) -> Outcome {
        self.stop_and_wait().await;
        let failed = forget(&self.dir, false).err().map(|e| {
            let path = self.dir.join(KEY_FILE);
            format!(
                "This device was signed out, but {} couldn't be deleted ({e}). Delete it, then start again.",
                path.display()
            )
        });
        Outcome::SignedOut { note, username: username(&self.dir), failed }
    }

    /// The account is deleted: stops the core and deletes the key file, the
    /// username and the saved chats, then leaves.
    async fn leave(&mut self, mut notes: Vec<String>) -> Flow {
        self.stop_and_wait().await;
        if let Err(e) = forget(&self.dir, true) {
            notes.push(format!(
                "Some of its files here couldn't be deleted ({e}): delete {} by hand.",
                self.dir.display()
            ));
        }
        Flow::Leave(notes)
    }

    /// Asks the running core without waiting: the command is queued in
    /// order now, and its reply, with what was asked, comes out of
    /// [`Runner::next`]. A new password's key file is saved before its
    /// reply goes on.
    fn call(&mut self, asked: Command) {
        let answer = self.driver.as_ref().map(|driver| driver.ask(asked.clone()));
        let reply_to = self.reply_to.clone();
        let key_file = matches!(asked, Command::ChangePassword { .. }).then(|| self.dir.join(KEY_FILE));
        self.calls += 1;
        tokio::spawn(async move {
            let mut reply = match answer {
                Some(answer) => answer.await.map_err(|e| say(&e)),
                None => Err(say(&Error::Stopped)),
            };
            if let (Some(path), Ok(Reply::KeyFile(key))) = (&key_file, &reply)
                && let Err(e) = write_private(path, key.to_json().as_bytes())
            {
                reply = Err(format!(
                    "Your password was changed, but the key file here couldn't be saved ({e}). \
                     This computer still unlocks with your old password."
                ));
            }
            // The runner keeps the receiver as long as it lives.
            let _ = reply_to.send(Outcome::Reply { asked, reply });
        });
    }

    async fn activate(&mut self, code: &Secret) -> Result<Outcome, String> {
        let driver = self.driver.as_ref().ok_or_else(|| say(&Error::Stopped))?;
        // The core takes a plain `String`: this copy is the only one outside `Zeroizing`.
        match driver.call(Command::Activate { code: code.expose().to_string() }).await {
            Ok(Reply::Account(account)) => Ok(running(account)),
            Ok(_) => Err(say(&Error::BadServerMessage)),
            Err(e) => Err(say(&e)),
        }
    }
}

/// The next of `held`, then of `updates`; waits forever once the core stops.
async fn next_update(held: &mut VecDeque<Update>, updates: &mut Option<UnboundedReceiver<Update>>) -> Update {
    if let Some(update) = held.pop_front() {
        return update;
    }
    if let Some(got) = updates.as_mut() {
        if let Some(update) = got.recv().await {
            return update;
        }
        // The driver stopped: nothing more will come.
        *updates = None;
    }
    std::future::pending().await
}

/// Running, or waiting for an invite code first.
fn running(account: Account) -> Outcome {
    if account.pending_until.is_some() { Outcome::Pending { account } } else { Outcome::Running { account } }
}

/// Deletes the key file and the saved chats, and the username if `username`.
/// What wasn't there is fine; what couldn't be deleted is the error.
fn forget(dir: &Path, username: bool) -> std::io::Result<()> {
    delete_cache(&dir.join(CACHE_FILE));
    let mut files = vec![KEY_FILE];
    if username {
        files.push(USERNAME_FILE);
    }
    for file in files {
        match fs::remove_file(dir.join(file)) {
            Err(e) if e.kind() != std::io::ErrorKind::NotFound => return Err(e),
            _ => {}
        }
    }
    Ok(())
}

/// Deletes the cache file and its `-wal` and `-shm` files.
fn delete_cache(path: &Path) {
    for end in ["", "-wal", "-shm"] {
        let mut file = path.as_os_str().to_owned();
        file.push(end);
        let _ = fs::remove_file(file);
    }
}

/// The screen and the runner joined the way the loop joins them, minus the
/// terminal, against the real server running in this process.
#[cfg(test)]
pub(crate) mod testing {
    use std::collections::VecDeque;
    use std::net::SocketAddr;
    use std::path::{Path, PathBuf};
    use std::sync::Arc;

    use corvo_chat_core::native::{Command, NativeTransport, Reply};
    use corvo_chat_core::{Error, Strength, hello, sign_up};
    use corvo_chat_proto::frames::{Device, DeviceKind};
    use corvo_chat_proto::{B64, Number};
    use corvo_chat_server::clock::Clock;
    use corvo_chat_server::db::{self, Db};
    use corvo_chat_server::hub::Hub;
    use corvo_chat_server::limits::Limits;
    use corvo_chat_server::{AppState, Timeouts, Tls, app, serve_with};
    use opaque_ke::ServerSetup;
    use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    use rustls::pki_types::{CertificateDer, PrivateKeyDer};
    use tempfile::TempDir;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::{TcpListener, TcpSocket, TcpStream};
    use tokio::task::JoinHandle;

    use super::{Effect, Flow, Msg, Runner, Server};
    use crate::files::{KEY_FILE, username};
    use crate::ui::start::Start;
    use crate::ui::state::{Screen, Secret, State};
    use crate::ui::update::update;
    use crate::ui::view::render;

    /// The fast password hash, for tests only.
    pub const STRENGTH: Strength = Strength::FastInsecure;
    pub const PASSWORD: &str = "correct horse battery";

    /// The real server, on a free port with a self-signed certificate the
    /// app trusts. Keeps its data folder alive.
    pub struct TestServer {
        pub server: Server,
        pub state: AppState,
        pub dir: TempDir,
    }

    impl TestServer {
        /// A server with the default limits, except signups per IP (every
        /// test app comes from 127.0.0.1) and messages per minute (tests
        /// send many at once). Needs a tokio runtime.
        pub async fn start() -> TestServer {
            let dir = tempfile::tempdir().expect("a temp dir");
            let conn = db::open(&dir.path().join(db::DB_FILE)).expect("a database");
            let state = AppState {
                db: Db::new(conn),
                setup: Arc::new(ServerSetup::new(&mut rand_core::OsRng)),
                // The system's, so the admin helpers (which use it too) agree.
                clock: Clock::system(),
                limits: Arc::new(Limits { signups_per_ip_day: 1_000, messages_per_minute: 1_000, ..Limits::default() }),
                hub: Hub::default(),
            };
            let (config, der) = self_signed();
            let listener = TcpListener::bind("127.0.0.1:0").await.expect("a free port");
            let port = listener.local_addr().expect("a bound address").port();
            tokio::spawn(serve_with(listener, Tls::Static(config), app(state.clone()), Timeouts::default()));
            let roots = corvo_chat_core::native::Roots::Only(vec![der]);
            TestServer { server: Server { url: format!("wss://localhost:{port}/ws"), roots }, state, dir }
        }

        /// An invite code for a pending account, as the server's admin command makes.
        pub fn invite(&self, number: Number) -> String {
            let printed = corvo_chat_server::admin::invite(self.dir.path(), &number.digits()).expect("an invite");
            printed.lines().next().expect("the code first").to_string()
        }

        /// Activates a pending account, as the server's admin command does.
        pub fn activate(&self, number: Number) {
            corvo_chat_server::admin::activate(self.dir.path(), &number.digits()).expect("the account activates");
        }
    }

    fn self_signed() -> (Arc<rustls::ServerConfig>, CertificateDer<'static>) {
        let cert = rcgen::generate_simple_self_signed(["localhost".to_string()]).expect("a test certificate");
        let der = cert.cert.der().clone();
        let key = PrivateKeyDer::Pkcs8(cert.signing_key.serialize_der().into());
        let config = rustls::ServerConfig::builder_with_provider(Arc::new(rustls::crypto::ring::default_provider()))
            .with_safe_default_protocol_versions()
            .expect("ring supports the defaults")
            .with_no_client_auth()
            .with_single_cert(vec![der.clone()], key)
            .expect("the test certificate is valid");
        (Arc::new(config), der)
    }

    pub struct App {
        pub state: State,
        pub runner: Runner<Vec<u8>>,
        /// The data folder.
        pub dir: PathBuf,
        /// What it printed on leaving after Delete account.
        pub left: Option<Vec<String>>,
    }

    impl App {
        /// Starts as `main` does, on the data folder `dir` (made if missing).
        pub fn open(dir: &Path, server: &TestServer) -> App {
            App::open_at(dir, server.server.clone())
        }

        /// Like `open`, signing up and in at `server`, which can be the test
        /// server through a [`Proxy`].
        pub fn open_at(dir: &Path, server: Server) -> App {
            std::fs::create_dir_all(dir).expect("the data folder");
            let start = Start::new(dir.join(KEY_FILE).exists(), username(dir), None);
            let runner = Runner::new(dir.to_path_buf(), server, STRENGTH, Vec::new());
            App { state: State::start(start), runner, dir: dir.to_path_buf(), left: None }
        }

        /// A new account on the data folder `dir`, signed up through the
        /// screens, invited with the admin's code, and on the main screen
        /// with the start window closed. Its display name is `base`.
        pub async fn active(dir: &Path, server: &TestServer, base: &str) -> App {
            App::active_at(dir, server, server.server.clone(), base).await
        }

        /// Like `active`, with the app talking to `at`.
        pub async fn active_at(dir: &Path, server: &TestServer, at: Server, base: &str) -> App {
            let mut app = App::pending_at(dir, at, base).await;
            let invite = server.invite(app.number());
            app.send(&invite).await;
            assert!(matches!(app.state.screen, Screen::Main), "on the main screen");
            app.state.overlay = None;
            app
        }

        /// A new account signed up through the screens, on the code screen.
        pub async fn pending(dir: &Path, server: &TestServer, base: &str) -> App {
            App::pending_at(dir, server.server.clone(), base).await
        }

        /// Like `pending`, with the app talking to `at`.
        async fn pending_at(dir: &Path, at: Server, base: &str) -> App {
            let mut app = App::open_at(dir, at);
            for answer in ["u", base, base, PASSWORD, PASSWORD] {
                app.send(answer).await;
            }
            let code = app.recovery_code().expect("the recovery code is shown");
            app.send("").await;
            app.send(&code[code.len() - 4..]).await;
            app.until("online", |s| s.conn == Some(corvo_chat_core::ConnState::Online)).await;
            app
        }

        /// A new account invited by `inviter`'s app, so the two are
        /// contacts, on the main screen with the start window closed.
        pub async fn invited(dir: &Path, server: &TestServer, base: &str, inviter: &mut App) -> App {
            let mut app = App::pending_at(dir, server.server.clone(), base).await;
            let invite = match inviter.call(Command::Invite { number: app.number() }).await {
                Ok(Reply::Invite(invite)) => invite,
                other => panic!("no invite: {other:?}"),
            };
            app.send(&invite.code).await;
            assert!(matches!(app.state.screen, Screen::Main), "on the main screen");
            app.state.overlay = None;
            app
        }

        /// Opens the data folder `dir` again and unlocks it, talking to `at`.
        pub async fn unlock(dir: &Path, at: Server) -> App {
            let mut app = App::open_at(dir, at);
            app.send(PASSWORD).await;
            assert!(matches!(app.state.screen, Screen::Main), "unlocked");
            app.state.overlay = None;
            app
        }

        /// Asks the running core straight, as setup for a test.
        pub async fn call(&self, command: Command) -> Result<Reply, Error> {
            self.runner.driver.as_ref().expect("the core is running").call(command).await
        }

        /// A new account made straight with the core, still pending, on a
        /// device that's gone: its username and recovery code.
        pub async fn signed_up_elsewhere(server: &TestServer, base: &str) -> (String, Secret) {
            let mut t = NativeTransport::connect(&server.server.url, server.server.roots.clone()).await.unwrap();
            hello(&mut t, "0.0.0").await.unwrap();
            let device = Device { id: B64(rand::random()), kind: DeviceKind::Cli, name: "elsewhere".into() };
            let up = sign_up(&mut t, base, base, PASSWORD, &device, STRENGTH).await.expect("the signup goes through");
            (up.signed_in.account.username, Secret::new(up.recovery_code.display()))
        }

        /// The account's number: the pending one on the code screen, or
        /// the one on the status line.
        pub fn number(&self) -> Number {
            match &self.state.screen {
                Screen::Start(start) => start.pending().expect("on the code screen").number,
                Screen::Main => Number::parse(&self.state.number).expect("a number"),
            }
        }

        /// The recovery code, while it's shown.
        pub fn recovery_code(&self) -> Option<String> {
            let Screen::Start(start) = &self.state.screen else { return None };
            start.recovery_code().map(str::to_string)
        }

        /// How many server connections the runner opened itself (the core's
        /// don't count).
        pub fn connections(&self) -> usize {
            self.runner.connections
        }

        /// The screen, drawn at `w`×`h`, as text.
        pub fn screen(&self, w: u16, h: u16) -> String {
            let buf = render(&self.state, w, h);
            (0..buf.area.height)
                .map(|y| (0..buf.area.width).map(|x| buf[(x, y)].symbol()).collect::<String>())
                .collect::<Vec<_>>()
                .join("\n")
        }

        /// Feeds `msg` to `update` and carries out every effect that follows,
        /// waiting for the core to answer each call (20 seconds at most), so
        /// a test sees what came of it. `Some` with the message if the app exits.
        pub async fn handle(&mut self, msg: Msg) -> Option<String> {
            let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(20);
            let mut msg = Some(msg);
            loop {
                if let Some(msg) = msg.take() {
                    match self.handle_now(msg).await {
                        Ok(()) => {}
                        Err(ended) => return ended,
                    }
                }
                if self.runner.calls == 0 {
                    return None;
                }
                let reply = tokio::time::timeout_at(deadline, self.runner.replies.recv())
                    .await
                    .expect("the core answers in time")
                    .expect("the runner keeps its channel");
                self.runner.calls -= 1;
                msg = Some(Msg::Done(reply));
            }
        }

        /// Like `handle`, without waiting for calls to the core: their
        /// replies are left for `until`, as the loop leaves them for later.
        /// `Err` if the app ends, with the message it exits with.
        pub async fn handle_now(&mut self, msg: Msg) -> Result<(), Option<String>> {
            let mut queue: VecDeque<Effect> = update(&mut self.state, msg).into();
            while let Some(effect) = queue.pop_front() {
                match self.runner.apply(effect).await.expect("the terminal works") {
                    Flow::Next(Some(msg)) => queue.extend(update(&mut self.state, msg)),
                    Flow::Next(None) => {}
                    Flow::Quit => return Err(None),
                    Flow::Exit(why) => return Err(Some(why)),
                    Flow::Leave(notes) => {
                        self.left = Some(notes);
                        return Err(None);
                    }
                }
            }
            Ok(())
        }

        /// Calls to the core still waiting for their replies.
        pub fn calls(&self) -> usize {
            self.runner.calls
        }

        pub async fn press(&mut self, code: KeyCode) -> Option<String> {
            self.handle(Msg::Key(KeyEvent::new(code, KeyModifiers::NONE))).await
        }

        /// Types `text` and presses Enter.
        pub async fn send(&mut self, text: &str) -> Option<String> {
            for c in text.chars() {
                self.press(KeyCode::Char(c)).await;
            }
            self.press(KeyCode::Enter).await
        }

        /// Handles the core's updates and replies until `done` holds; fails
        /// after 20 seconds.
        pub async fn until(&mut self, what: &str, done: impl Fn(&State) -> bool) {
            let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(20);
            while !done(&self.state) {
                let msg = tokio::time::timeout_at(deadline, self.runner.next())
                    .await
                    .unwrap_or_else(|_| panic!("timed out waiting for {what}"));
                self.handle(msg).await;
            }
        }

        /// Stops the core, as quitting does.
        pub async fn close(mut self) {
            self.runner.shutdown().await;
        }
    }

    /// A TCP proxy in front of the test server that the test can cut
    /// (every connection dropped, the port refusing) and open again on the
    /// same port, as the core's driver tests do.
    pub struct Proxy {
        addr: SocketAddr,
        target: SocketAddr,
        accept: Option<JoinHandle<()>>,
        /// While set, what the app sends goes no further: the server never
        /// hears it, and so never answers.
        hold: Arc<std::sync::atomic::AtomicBool>,
    }

    impl Proxy {
        pub async fn start(server: &TestServer) -> Proxy {
            let port = server.server.url.rsplit(':').next().unwrap().trim_end_matches("/ws").parse::<u16>().unwrap();
            let target = SocketAddr::from(([127, 0, 0, 1], port));
            let addr = TcpListener::bind("127.0.0.1:0").await.unwrap().local_addr().unwrap();
            let mut proxy = Proxy { addr, target, accept: None, hold: Arc::default() };
            proxy.open();
            proxy
        }

        /// The test server, reached through this proxy.
        pub fn server(&self, server: &TestServer) -> Server {
            Server { url: format!("wss://localhost:{}/ws", self.addr.port()), roots: server.server.roots.clone() }
        }

        pub fn open(&mut self) {
            let socket = TcpSocket::new_v4().unwrap();
            socket.set_reuseaddr(true).unwrap();
            socket.bind(self.addr).unwrap();
            let listener = socket.listen(64).unwrap();
            let (target, hold) = (self.target, self.hold.clone());
            self.accept = Some(tokio::spawn(async move {
                // The connections live in this set, so aborting the task drops them all.
                let mut links = tokio::task::JoinSet::new();
                loop {
                    tokio::select! {
                        accepted = listener.accept() => {
                            let Ok((inbound, _)) = accepted else { return };
                            links.spawn(link(inbound, target, hold.clone()));
                        }
                        Some(_) = links.join_next() => {}
                    }
                }
            }));
        }

        /// Stalls the connections (`true`) or lets them through again.
        pub fn hold(&self, hold: bool) {
            self.hold.store(hold, std::sync::atomic::Ordering::SeqCst);
        }

        pub async fn cut(&mut self) {
            if let Some(task) = self.accept.take() {
                task.abort();
                let _ = task.await;
            }
        }
    }

    /// One proxied connection.
    async fn link(inbound: TcpStream, target: SocketAddr, hold: Arc<std::sync::atomic::AtomicBool>) {
        let Ok(outbound) = TcpStream::connect(target).await else { return };
        let ((mut client_in, mut client_out), (mut server_in, mut server_out)) =
            (inbound.into_split(), outbound.into_split());
        let up = async {
            let mut buf = [0u8; 4096];
            while let Ok(n @ 1..) = client_in.read(&mut buf).await {
                if hold.load(std::sync::atomic::Ordering::SeqCst) {
                    continue;
                }
                if server_out.write_all(&buf[..n]).await.is_err() {
                    return;
                }
            }
        };
        tokio::select! {
            _ = up => {}
            _ = tokio::io::copy(&mut server_in, &mut client_out) => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn runner() -> Runner<Vec<u8>> {
        Runner::new(PathBuf::from("data"), Server::default(), Strength::Normal, Vec::new())
    }

    #[test]
    fn the_default_server_is_the_public_one() {
        let server = Server::default();
        assert_eq!(server.url, "wss://chat.corvo.live/ws");
        assert!(matches!(server.roots, Roots::Web));
    }

    #[tokio::test]
    async fn the_title_goes_to_the_terminal_and_quitting_ends() {
        let mut runner = runner();
        assert!(matches!(runner.apply(Effect::SetTitle("corvo-chat".into())).await.unwrap(), Flow::Next(None)));
        assert!(String::from_utf8_lossy(&runner.out).contains("corvo-chat"), "an escape code with the title");
        assert!(matches!(runner.apply(Effect::Quit).await.unwrap(), Flow::Quit));
        let exit = runner.apply(Effect::Exit("broken".into())).await.unwrap();
        assert!(matches!(exit, Flow::Exit(why) if why == "broken"));
    }

    #[tokio::test]
    async fn a_call_returns_at_once_and_its_reply_comes_later() {
        let mut runner = runner();
        assert!(matches!(runner.apply(Effect::Call(Command::Chats)).await.unwrap(), Flow::Next(None)));
        let reply = tokio::time::timeout(std::time::Duration::from_secs(5), runner.next()).await.unwrap();
        assert!(
            matches!(&reply, Msg::Done(Outcome::Reply { asked: Command::Chats, reply: Err(_) })),
            "with no core, it says so: {reply:?}"
        );
        assert_eq!(runner.calls, 0);
    }

    #[tokio::test]
    async fn no_updates_come_before_the_core_runs() {
        let mut runner = runner();
        let waited = tokio::time::timeout(std::time::Duration::from_millis(50), runner.next()).await;
        assert!(waited.is_err(), "still waiting");
    }
}
