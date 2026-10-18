//! Carries out [`Effect`]s: the core, the files and the terminal. What came
//! of each goes back to `update` as [`Msg::Done`].
//!
//! The start screens' effects run the account flows on a connection of their
//! own, save the key file, then start the core's driver, which the runner
//! keeps from then on. Its updates come out of [`Runner::next_update`].

use std::collections::VecDeque;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use corvo_chat_core::native::{Command, Driver, NativeTransport, Reply, Roots, first_start};
use corvo_chat_core::{
    ConnState, Error, LocalKey, Recovery, SignedIn, SignedUp, Strength, Update, hello, sign_in, sign_up,
};
use corvo_chat_proto::B64;
use corvo_chat_proto::frames::{Account, Device, DeviceKind};
use ratatui::crossterm::execute;
use ratatui::crossterm::terminal::SetTitle;
use tokio::sync::mpsc::UnboundedReceiver;

use super::start::say;
use super::state::{Secret, VERSION};
use super::update::{Effect, Msg, Outcome};
use crate::files::{CACHE_FILE, KEY_FILE, USERNAME_FILE, device_id, device_name, write_private};

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
    #[cfg_attr(not(test), expect(dead_code, reason = "deleting the account leaves this way"))]
    Leave(Vec<String>),
}

/// Said when the first connection of a key file never made it.
const NO_SERVER: &str = "Couldn't reach the server. Check your connection and try again.";
/// Said when the server needs a newer app.
const TOO_OLD: &str = "This version is too old. Update corvo-chat.";

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
    /// Connections opened, for tests.
    #[cfg(test)]
    connections: usize,
}

impl<W: Write> Runner<W> {
    /// `dir` is the data folder; `server` is where to sign up and sign in.
    pub fn new(dir: PathBuf, server: Server, strength: Strength, out: W) -> Runner<W> {
        Runner {
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
        };
        Ok(Flow::Next(Some(Msg::Done(outcome.unwrap_or_else(Outcome::Failed)))))
    }

    /// The core's next update; waits forever while it isn't running.
    pub async fn next_update(&mut self) -> Option<Update> {
        if let Some(update) = self.held.pop_front() {
            return Some(update);
        }
        let got = match self.updates.as_mut() {
            Some(updates) => updates.recv().await,
            None => None,
        };
        if got.is_none() {
            // The driver stopped: nothing more will come.
            self.updates = None;
            return std::future::pending().await;
        }
        got
    }

    /// Stops the core.
    pub async fn shutdown(&mut self) {
        self.stop();
    }

    fn stop(&mut self) {
        // Dropping the driver stops it.
        self.driver = None;
        self.updates = None;
        self.held.clear();
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
    /// the server the key file names.
    async fn unlock(&mut self, password: &Secret) -> Result<Outcome, String> {
        let json =
            fs::read_to_string(self.dir.join(KEY_FILE)).map_err(|e| format!("Couldn't read your key file: {e}"))?;
        let key = LocalKey::from_json(&json).map_err(|e| say(&e))?;
        let device = Device { id: B64(key.device_id), kind: DeviceKind::Cli, name: device_name() };
        self.run(key, password, device)?;
        match self.account().await {
            Ok(account) => Ok(running(account)),
            Err(why) => {
                self.stop();
                Err(why)
            }
        }
    }

    /// The account as the core saved it. A key file whose first connection
    /// never made it has none saved: then this waits for the first one.
    async fn account(&mut self) -> Result<Account, String> {
        let mut waited = false;
        loop {
            let driver = self.driver.as_ref().ok_or_else(|| say(&Error::Stopped))?;
            match driver.call(Command::Account).await {
                Ok(Reply::Account(account)) => return Ok(account),
                Ok(_) => return Err(say(&Error::BadServerMessage)),
                Err(Error::Offline) if !waited => waited = true,
                Err(e) => return Err(say(&e)),
            }
            let updates = self.updates.as_mut().ok_or_else(|| say(&Error::Stopped))?;
            loop {
                let update = updates.recv().await.ok_or_else(|| say(&Error::Stopped))?;
                match update {
                    Update::Connection(ConnState::Online) => {
                        self.held.push_back(update);
                        break;
                    }
                    Update::Connection(ConnState::UpdateRequired) => return Err(TOO_OLD.into()),
                    Update::Connection(_) => return Err(NO_SERVER.into()),
                    Update::SignedOut(reason) => return Err(say(&Error::SignedOut(reason))),
                    other => self.held.push_back(other),
                }
            }
        }
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

/// Running, or waiting for an invite code first.
fn running(account: Account) -> Outcome {
    if account.pending_until.is_some() { Outcome::Pending { account } } else { Outcome::Running { account } }
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
#[allow(dead_code, reason = "a harness: each task's tests use only some of it")]
pub(crate) mod testing {
    use std::collections::VecDeque;
    use std::path::{Path, PathBuf};
    use std::sync::Arc;

    use corvo_chat_core::native::NativeTransport;
    use corvo_chat_core::{Strength, hello, sign_up};
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
    use tokio::net::TcpListener;

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
        /// A server with the default limits, except signups per IP: every
        /// test app comes from 127.0.0.1. Needs a tokio runtime.
        pub async fn start() -> TestServer {
            let dir = tempfile::tempdir().expect("a temp dir");
            let conn = db::open(&dir.path().join(db::DB_FILE)).expect("a database");
            let state = AppState {
                db: Db::new(conn),
                setup: Arc::new(ServerSetup::new(&mut rand_core::OsRng)),
                // The system's, so the admin helpers (which use it too) agree.
                clock: Clock::system(),
                limits: Arc::new(Limits { signups_per_ip_day: 1_000, ..Limits::default() }),
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
            std::fs::create_dir_all(dir).expect("the data folder");
            let start = Start::new(dir.join(KEY_FILE).exists(), username(dir), None);
            let runner = Runner::new(dir.to_path_buf(), server.server.clone(), STRENGTH, Vec::new());
            App { state: State::start(start), runner, dir: dir.to_path_buf(), left: None }
        }

        /// A new account on the data folder `dir`, signed up through the
        /// screens, invited with the admin's code, and on the main screen
        /// with the start window closed. Its display name is `base`.
        pub async fn active(dir: &Path, server: &TestServer, base: &str) -> App {
            let mut app = App::open(dir, server);
            for answer in ["u", base, base, PASSWORD, PASSWORD] {
                app.send(answer).await;
            }
            let code = app.recovery_code().expect("the recovery code is shown");
            app.send("").await;
            app.send(&code[code.len() - 4..]).await;
            app.until("online", |s| s.conn == Some(corvo_chat_core::ConnState::Online)).await;
            let invite = server.invite(app.number());
            app.send(&invite).await;
            assert!(matches!(app.state.screen, Screen::Main), "on the main screen");
            app.state.overlay = None;
            app
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

        /// Feeds `msg` to `update` and carries out every effect that follows.
        /// `Some` with the message if the app exits.
        pub async fn handle(&mut self, msg: Msg) -> Option<String> {
            let mut queue: VecDeque<Effect> = update(&mut self.state, msg).into();
            while let Some(effect) = queue.pop_front() {
                match self.runner.apply(effect).await.expect("the terminal works") {
                    Flow::Next(Some(msg)) => queue.extend(update(&mut self.state, msg)),
                    Flow::Next(None) => {}
                    Flow::Quit => return None,
                    Flow::Exit(why) => return Some(why),
                    Flow::Leave(notes) => {
                        self.left = Some(notes);
                        return None;
                    }
                }
            }
            None
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

        /// Handles the core's updates until `done` holds; fails after 20 seconds.
        pub async fn until(&mut self, what: &str, done: impl Fn(&State) -> bool) {
            let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(20);
            while !done(&self.state) {
                let update = tokio::time::timeout_at(deadline, self.runner.next_update())
                    .await
                    .unwrap_or_else(|_| panic!("timed out waiting for {what}"))
                    .expect("the core is running");
                self.handle(Msg::Core(update)).await;
            }
        }

        /// Stops the core, as quitting does.
        pub async fn close(mut self) {
            self.runner.shutdown().await;
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
    async fn no_updates_come_before_the_core_runs() {
        let mut runner = runner();
        let waited = tokio::time::timeout(std::time::Duration::from_millis(50), runner.next_update()).await;
        assert!(waited.is_err(), "still waiting");
    }
}
