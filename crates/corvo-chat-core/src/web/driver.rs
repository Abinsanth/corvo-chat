//! The browser driver: the native driver's loop on the page's own event
//! loop. It starts from the account secrets and session token, then
//! connects, resumes and syncs, reconnecting with back-off (1, 2, 4 … 30
//! seconds, reset once synced) whenever the connection drops, and answers
//! from the saved chats meanwhile. The app talks to it through commands and
//! hears back through updates. A sign-out stops it for good and deletes the
//! saved chats.
//!
//! The browser never shows the page the server's pings, so the driver
//! checks a connection that has gone quiet itself. If the saved chats stop
//! being stored (the browser's storage is full, say), it keeps going from
//! memory: the next page load syncs in full.

use std::cell::Cell;
use std::future::Future;
use std::rc::Rc;

use corvo_chat_proto::frames::{Account, ClientFrame, Device, ServerFrame};
use futures::channel::{mpsc, oneshot};
use futures::{FutureExt, StreamExt};
use gloo_timers::future::TimeoutFuture;
use zeroize::Zeroizing;

use super::{IdbCache, WebTransport, now_ms, ws::CONNECT_TIMEOUT_MS};
use crate::cache::Cache;
use crate::client::Client;
use crate::commands::{self, Command, Reply};
use crate::keyfile::KeyFile;
use crate::policy::{self, End, Next};
use crate::sync::{ConnState, Update};
use crate::{Error, Secrets, SignedIn, Strength, Transport, TransportError, hello, resume};

/// How long a connection may go without a frame from the server before
/// the driver checks it.
const QUIET_MS: u64 = 60_000;
/// How long that check may take before the connection counts as lost.
const CHECK_TIMEOUT_MS: u64 = 10_000;

/// The live client the driver runs.
type Live = Client<Watched, IdbCache>;

type Call = (Command, oneshot::Sender<Result<Reply, Error>>);

/// The handle to a running driver. Dropping it stops the driver.
pub struct WebDriver {
    calls: mpsc::UnboundedSender<Call>,
    wakes: mpsc::UnboundedSender<()>,
}

impl WebDriver {
    /// Starts the driver on the page's event loop, with the session that
    /// `token` resumes and the chats saved in `cache`. It connects to `url`.
    /// `app_version` is what `hello` tells the server.
    pub fn start(
        url: String,
        secrets: Secrets,
        token: Zeroizing<[u8; 32]>,
        username: String,
        device: Device,
        cache: IdbCache,
        app_version: String,
    ) -> (WebDriver, mpsc::UnboundedReceiver<Update>) {
        let (calls, calls_rx) = mpsc::unbounded();
        let (wakes, wakes_rx) = mpsc::unbounded();
        let (updates, updates_rx) = mpsc::unbounded();
        let key_file =
            KeyFile { server: url.clone(), username, device_id: device.id.0, token, strength: Strength::Normal };
        let task = Task { url, app_version, device, secrets, key_file, calls: calls_rx, updates };
        wasm_bindgen_futures::spawn_local(task.run(cache, wakes_rx));
        (WebDriver { calls, wakes }, updates_rx)
    }

    /// Runs a command and gives its reply. `Stopped` once the driver has stopped.
    pub async fn call(&self, command: Command) -> Result<Reply, Error> {
        self.ask(command).await
    }

    /// Queues a command now, in order with the others, and gives its reply
    /// later. The future doesn't hold on to the driver, so an app can await
    /// it elsewhere; it gives `Stopped` if the driver stops first.
    pub fn ask(&self, command: Command) -> impl Future<Output = Result<Reply, Error>> + 'static {
        let (reply, answer) = oneshot::channel();
        let sent = self.calls.unbounded_send((command, reply)).is_ok();
        async move {
            if !sent {
                return Err(Error::Stopped);
            }
            answer.await.map_err(|_| Error::Stopped)?
        }
    }

    /// For when the page comes back (shown again, or back online): waiting
    /// out a back-off, the driver connects now; online, it checks the
    /// connection still works.
    pub fn wake(&self) {
        let _ = self.wakes.unbounded_send(());
    }
}

/// The connection, noting when the server last sent a frame.
struct Watched {
    t: WebTransport,
    heard: Rc<Cell<u64>>,
}

impl Transport for Watched {
    async fn send(&mut self, frame: &ClientFrame) -> Result<(), TransportError> {
        self.t.send(frame).await
    }

    async fn recv(&mut self) -> Result<ServerFrame, TransportError> {
        let frame = self.t.recv().await;
        if frame.is_ok() {
            self.heard.set(now_ms());
        }
        frame
    }
}

/// A timer for `ms` milliseconds.
fn timer(ms: u64) -> TimeoutFuture {
    // The browser's longest timer.
    TimeoutFuture::new(ms.min(i32::MAX as u64) as u32)
}

/// `f`'s output, or `None` if it takes longer than `ms` milliseconds.
async fn within<F: Future>(ms: u64, f: F) -> Option<F::Output> {
    futures::select! {
        out = f.fuse() => Some(out),
        () = timer(ms).fuse() => None,
    }
}

fn late() -> TransportError {
    TransportError::Io("The server didn't answer in time.".into())
}

/// Opens a connection and resumes this device's session.
async fn connect(
    url: String,
    app_version: String,
    token: Zeroizing<[u8; 32]>,
    device_id: [u8; 16],
) -> Result<(WebTransport, Account), Error> {
    let mut t = WebTransport::connect(&url).await?;
    let account = within(CONNECT_TIMEOUT_MS.into(), async {
        hello(&mut t, &app_version).await?;
        resume(&mut t, &token, &device_id).await
    })
    .await
    .ok_or_else(late)??;
    Ok((t, account))
}

/// A sync that must end within [`policy::FIRST_SYNC_TIMEOUT_MS`]; a
/// connection that stalls here is lost like a dropped one.
async fn first_sync(client: &mut Live) -> Result<(), Error> {
    within(policy::FIRST_SYNC_TIMEOUT_MS, client.sync()).await.unwrap_or_else(|| Err(late().into()))
}

/// The next wake. Once the app has dropped the driver it never comes: the
/// calls closing stops the driver, after it answers those already queued.
async fn woken(wakes: &mut mpsc::UnboundedReceiver<()>) {
    if wakes.next().await.is_none() {
        std::future::pending::<()>().await;
    }
}

/// Waits `secs` seconds, or less if the app wakes the driver.
async fn wait_or_wake(wakes: &mut mpsc::UnboundedReceiver<()>, secs: u64) {
    within(secs * 1000, woken(wakes)).await;
}

/// Forgets the wakes already handled: several in a row are one.
fn forget(wakes: &mut mpsc::UnboundedReceiver<()>) {
    while wakes.try_recv().is_ok() {}
}

/// A copy of the secrets for the next connection's client.
fn copy(secrets: &Secrets) -> Secrets {
    Secrets::from_bytes(secrets.to_bytes().as_ref()).expect("secrets always read back")
}

/// What woke the live loop.
enum Event {
    Frame(Result<Vec<Update>, Error>),
    Call(Call),
    /// Quiet for too long, or the app woke the driver.
    Check,
    /// The app dropped the driver.
    Dropped,
}

/// The driver's own state, owned by its task.
struct Task {
    url: String,
    app_version: String,
    device: Device,
    secrets: Secrets,
    key_file: KeyFile,
    calls: mpsc::UnboundedReceiver<Call>,
    updates: mpsc::UnboundedSender<Update>,
}

impl Task {
    fn tell(&self, update: Update) {
        // An app that stopped listening still gets its commands answered.
        let _ = self.updates.unbounded_send(update);
    }

    async fn run(mut self, mut cache: IdbCache, mut wakes: mpsc::UnboundedReceiver<()>) {
        let mut backoff = 1;
        loop {
            // A wake from here on ends the next back-off at once.
            forget(&mut wakes);
            let attempt = connect(
                self.url.clone(),
                self.app_version.clone(),
                self.key_file.token.clone(),
                self.key_file.device_id,
            );
            let end = match self.offline_until(&mut cache, attempt).await {
                None => End::Shutdown,
                Some(Err(e)) => End::from(e),
                Some(Ok((t, account))) => {
                    let heard = Rc::new(Cell::new(now_ms()));
                    let t = Watched { t, heard: heard.clone() };
                    let signed_in =
                        SignedIn { token: self.key_file.token.clone(), account, secrets: copy(&self.secrets) };
                    let mut client = Client::new(t, signed_in, self.device.clone(), self.key_file.strength, cache);
                    let end = self.live(&mut client, &heard, &mut backoff, &mut wakes).await;
                    client.take_updates().into_iter().for_each(|u| self.tell(u));
                    cache = client.into_cache();
                    end
                }
            };
            match (policy::after_end(&end), end) {
                (Next::Stop, End::SignedOut(reason)) => {
                    let name = cache.name().to_owned();
                    // Closed before its database goes.
                    drop(cache);
                    let _ = IdbCache::delete(&name).await;
                    self.tell(Update::SignedOut(reason));
                    return;
                }
                (Next::Stop, _) => return,
                (Next::StayOffline, _) => {
                    // Told once; what's saved can still be read until the page closes.
                    self.tell(Update::Connection(ConnState::UpdateRequired));
                    self.offline_until(&mut cache, std::future::pending::<()>()).await;
                    return;
                }
                (Next::Reconnect, _) => {
                    self.tell(Update::Connection(ConnState::Reconnecting { in_secs: backoff }));
                    let wait = wait_or_wake(&mut wakes, backoff);
                    if self.offline_until(&mut cache, wait).await.is_none() {
                        return;
                    }
                    backoff = policy::next_backoff(backoff);
                }
            }
        }
    }

    /// Waits for `until`, answering commands from the cache meanwhile.
    /// `None` if the app dropped the driver.
    async fn offline_until<F: Future>(&mut self, cache: &mut IdbCache, until: F) -> Option<F::Output> {
        let until = until.fuse();
        let mut until = std::pin::pin!(until);
        loop {
            let (command, reply) = futures::select! {
                out = until => return Some(out),
                call = self.calls.next() => call?,
            };
            let sending = matches!(command, Command::SendText { .. });
            let answer = commands::offline(cache, &self.secrets, command, &commands::at(now_ms()));
            if sending {
                // Stored before the app hears it's queued. A failed store
                // still sends it from memory once online.
                let _ = cache.flush().await;
            }
            let _ = reply.send(answer);
        }
    }

    /// Syncs a new connection, then runs it until it ends. A pending account
    /// can't sync, so it waits for `Activate` first.
    async fn live(
        &mut self,
        client: &mut Live,
        heard: &Cell<u64>,
        backoff: &mut u64,
        wakes: &mut mpsc::UnboundedReceiver<()>,
    ) -> End {
        if client.account().pending_until.is_none()
            && let Err(e) = first_sync(client).await
        {
            return End::from(e);
        }
        *backoff = 1;
        self.tell(Update::Connection(ConnState::Online));
        forget(wakes);
        loop {
            let quiet = QUIET_MS.saturating_sub(now_ms().saturating_sub(heard.get()));
            // `next` is cancel-safe; a command, once taken, runs to the end.
            let event = futures::select! {
                updates = client.next().fuse() => Event::Frame(updates),
                call = self.calls.next() => call.map_or(Event::Dropped, Event::Call),
                () = woken(wakes).fuse() => Event::Check,
                () = timer(quiet).fuse() => Event::Check,
            };
            let end = match event {
                Event::Frame(Ok(updates)) => {
                    updates.into_iter().for_each(|u| self.tell(u));
                    None
                }
                // An error no request was waiting for: nothing to answer.
                Event::Frame(Err(Error::Server { .. })) => None,
                Event::Frame(Err(e)) => Some(End::from(e)),
                Event::Call((command, reply)) => self.command(client, command, reply).await,
                Event::Check => {
                    let end = check(client, &self.key_file).await;
                    // The wakes that came meanwhile are answered by this check.
                    forget(wakes);
                    end
                }
                Event::Dropped => Some(End::Shutdown),
            };
            if let Some(end) = end {
                return end;
            }
        }
    }

    /// Runs one command to the end and answers it. Gives why the connection
    /// ends, if it does: a sign-out, a broken connection, or a reply that
    /// never came or made no sense (a reply out of step can't be trusted).
    async fn command(
        &mut self,
        client: &mut Live,
        command: Command,
        reply: oneshot::Sender<Result<Reply, Error>>,
    ) -> Option<End> {
        // Only a pending account syncs once active; one already synced doesn't again.
        let was_pending = client.account().pending_until.is_some();
        let sending = matches!(command, Command::SendText { .. });
        let limit = policy::time_limit_ms(&command);
        let new = commands::at(now_ms());
        // What the policy needs of the command once it has been run.
        let spent = command.clone();
        let run = commands::online(client, command, &self.key_file, &new);
        let (result, timed_out) = match within(limit, run).await {
            Some(result) => (result, false),
            None => (Err(Error::Offline), true),
        };
        let end = policy::end_after(&spent, &result, timed_out);
        // A message that made it into the outbox goes later whatever happened
        // here, so the app gets its id, not an error that invites a retype.
        let queued =
            sending && result.is_err() && client.cache().outbox().is_ok_and(|o| o.iter().any(|o| o.msg_id == new.id));
        let result = if queued { Ok(Reply::Sent(new.id)) } else { result };
        if sending {
            // Stored before the app hears of it, as offline.
            let _ = client.cache().flush().await;
        }
        let synced_now = policy::syncs_after(was_pending, &spent, &result);
        let _ = reply.send(result);
        if end.is_none() && synced_now {
            // Active now, so it can sync.
            return first_sync(client).await.err().map(End::from);
        }
        end
    }
}

/// Asks for the devices, the cheapest request there is, to see that the
/// connection still works. No answer in time means it doesn't.
async fn check(client: &mut Live, key_file: &KeyFile) -> Option<End> {
    let new = commands::at(now_ms());
    let run = commands::online(client, Command::Devices, key_file, &new);
    let (result, timed_out) = match within(CHECK_TIMEOUT_MS, run).await {
        Some(result) => (result, false),
        None => (Err(Error::Offline), true),
    };
    policy::end_after(&Command::Devices, &result, timed_out)
}
