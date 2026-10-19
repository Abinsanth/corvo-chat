//! The driver: a tokio task that keeps the client connected. It unlocks the
//! key file offline, opens the saved chats, then connects, resumes and syncs,
//! reconnecting with back-off (1, 2, 4 … 30 seconds, reset once synced)
//! whenever the connection drops.
//! The app talks to it through commands and hears back through updates. A
//! sign-out stops it for good and deletes the saved chats.

use std::future::Future;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use corvo_chat_proto::frames::{Account, Device};
use tokio::sync::{mpsc, oneshot};
use zeroize::Zeroizing;

use super::{NativeTransport, Roots, ws::CONNECT_TIMEOUT};
use crate::cache::{Cache, SqliteCache};
use crate::client::Client;
use crate::commands::{self, Command, Reply};
use crate::keyfile::KeyFile;
use crate::policy::{self, End, Next};
use crate::sync::{ConnState, Update};
use crate::{Error, LocalKey, Secrets, SignedIn, Strength, TransportError, hello, resume};

/// The live client the driver runs.
type Live = Client<NativeTransport, SqliteCache>;

type Call = (Command, oneshot::Sender<Result<Reply, Error>>);

/// The handle to a running driver. Dropping it stops the driver.
pub struct Driver {
    calls: mpsc::UnboundedSender<Call>,
}

impl Driver {
    /// Unlocks the key file (no network needed), opens the saved chats at
    /// `cache_path`, and starts the driver on the current tokio runtime. It
    /// connects to the server the key file names, the one its session is
    /// for. A wrong password or another account's cache fails here.
    /// `app_version` is what `hello` tells the server.
    pub fn start(
        roots: Roots,
        key: LocalKey,
        password: &str,
        cache_path: PathBuf,
        device: Device,
        s: Strength,
        app_version: &str,
    ) -> Result<(Driver, mpsc::UnboundedReceiver<Update>), Error> {
        let (secrets, token) = key.unlock(password)?;
        let cache = SqliteCache::open(&cache_path, &secrets)?;
        let (calls, calls_rx) = mpsc::unbounded_channel();
        let (updates, updates_rx) = mpsc::unbounded_channel();
        let url = key.server;
        let key_file =
            KeyFile { server: url.clone(), username: key.username, device_id: key.device_id, token, strength: s };
        let task = Task {
            url,
            roots,
            app_version: app_version.into(),
            device,
            secrets,
            key_file,
            path: cache_path,
            calls: calls_rx,
            updates,
        };
        tokio::spawn(task.run(cache));
        Ok((Driver { calls }, updates_rx))
    }

    /// Runs a command and gives its reply. `Stopped` once the driver has stopped.
    pub async fn call(&self, command: Command) -> Result<Reply, Error> {
        self.ask(command).await
    }

    /// Queues a command now, in order with the others, and gives its reply
    /// later. The future doesn't hold on to the driver, so an app can await
    /// it elsewhere; it gives `Stopped` if the driver stops first.
    pub fn ask(&self, command: Command) -> impl Future<Output = Result<Reply, Error>> + Send + 'static {
        let (reply, answer) = oneshot::channel();
        let sent = self.calls.send((command, reply)).is_ok();
        async move {
            if !sent {
                return Err(Error::Stopped);
            }
            answer.await.map_err(|_| Error::Stopped)?
        }
    }
}

/// Opens a connection and resumes this device's session.
async fn connect(
    url: String,
    roots: Roots,
    app_version: String,
    token: Zeroizing<[u8; 32]>,
    device_id: [u8; 16],
) -> Result<(NativeTransport, Account), Error> {
    let mut t = NativeTransport::connect(&url, roots).await?;
    let account = tokio::time::timeout(CONNECT_TIMEOUT, async {
        hello(&mut t, &app_version).await?;
        resume(&mut t, &token, &device_id).await
    })
    .await
    .map_err(|_| TransportError::Io("The server didn't answer in time.".into()))??;
    Ok((t, account))
}

/// A sync that must end within [`policy::FIRST_SYNC_TIMEOUT_MS`]; a
/// connection that stalls here is lost like a dropped one.
async fn first_sync(client: &mut Live) -> Result<(), Error> {
    tokio::time::timeout(Duration::from_millis(policy::FIRST_SYNC_TIMEOUT_MS), client.sync())
        .await
        .unwrap_or_else(|_| Err(TransportError::Io("The server didn't answer in time.".into()).into()))
}

/// A copy of the secrets for the next connection's client.
fn copy(secrets: &Secrets) -> Secrets {
    Secrets::from_bytes(secrets.to_bytes().as_ref()).expect("secrets always read back")
}

/// Deletes the cache file and its `-wal` and `-shm` files.
fn delete_cache(path: &Path) {
    for end in ["", "-wal", "-shm"] {
        let mut file = path.as_os_str().to_owned();
        file.push(end);
        let _ = std::fs::remove_file(file);
    }
}

/// The driver's own state, owned by its task.
struct Task {
    url: String,
    roots: Roots,
    app_version: String,
    device: Device,
    secrets: Secrets,
    key_file: KeyFile,
    path: PathBuf,
    calls: mpsc::UnboundedReceiver<Call>,
    updates: mpsc::UnboundedSender<Update>,
}

impl Task {
    fn tell(&self, update: Update) {
        // An app that stopped listening still gets its commands answered.
        let _ = self.updates.send(update);
    }

    async fn run(mut self, mut cache: SqliteCache) {
        let mut backoff = 1;
        loop {
            let attempt = connect(
                self.url.clone(),
                self.roots.clone(),
                self.app_version.clone(),
                self.key_file.token.clone(),
                self.key_file.device_id,
            );
            let end = match self.offline_until(&mut cache, attempt).await {
                None => End::Shutdown,
                Some(Err(e)) => End::from(e),
                Some(Ok((t, account))) => {
                    let signed_in =
                        SignedIn { token: self.key_file.token.clone(), account, secrets: copy(&self.secrets) };
                    let mut client = Client::new(t, signed_in, self.device.clone(), self.key_file.strength, cache);
                    let end = self.live(&mut client, &mut backoff).await;
                    client.take_updates().into_iter().for_each(|u| self.tell(u));
                    cache = client.into_cache();
                    end
                }
            };
            match (policy::after_end(&end), end) {
                (Next::Stop, End::SignedOut(reason)) => {
                    // Closed before its files go.
                    drop(cache);
                    delete_cache(&self.path);
                    self.tell(Update::SignedOut(reason));
                    return;
                }
                (Next::Stop, _) => return,
                (Next::StayOffline, _) => {
                    // Told once; what's saved can still be read until the app quits.
                    self.tell(Update::Connection(ConnState::UpdateRequired));
                    self.offline_until(&mut cache, std::future::pending::<()>()).await;
                    return;
                }
                (Next::Reconnect, _) => {
                    self.tell(Update::Connection(ConnState::Reconnecting { in_secs: backoff }));
                    let wait = tokio::time::sleep(Duration::from_secs(backoff));
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
    async fn offline_until<F: Future>(&mut self, cache: &mut SqliteCache, until: F) -> Option<F::Output> {
        let mut until = std::pin::pin!(until);
        loop {
            tokio::select! {
                out = &mut until => return Some(out),
                call = self.calls.recv() => {
                    let (command, reply) = call?;
                    let _ = reply.send(commands::offline(cache, &self.secrets, command, &commands::at(now_ms())));
                }
            }
        }
    }

    /// Syncs a new connection, then runs it until it ends. A pending account
    /// can't sync, so it waits for `Activate` first.
    async fn live(&mut self, client: &mut Live, backoff: &mut u64) -> End {
        if client.account().pending_until.is_none()
            && let Err(e) = first_sync(client).await
        {
            return End::from(e);
        }
        *backoff = 1;
        self.tell(Update::Connection(ConnState::Online));
        loop {
            // `next` is cancel-safe; a command, once taken, runs to the end.
            tokio::select! {
                updates = client.next() => match updates {
                    Ok(updates) => updates.into_iter().for_each(|u| self.tell(u)),
                    // An error no request was waiting for: nothing to answer.
                    Err(Error::Server { .. }) => {}
                    Err(e) => return End::from(e),
                },
                call = self.calls.recv() => {
                    let Some((command, reply)) = call else { return End::Shutdown };
                    if let Some(end) = self.command(client, command, reply).await {
                        return end;
                    }
                }
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
        let limit = Duration::from_millis(policy::time_limit_ms(&command));
        let new = commands::at(now_ms());
        // What the policy needs of the command once it has been run.
        let spent = command.clone();
        let run = commands::online(client, command, &self.key_file, &new);
        let (result, timed_out) = match tokio::time::timeout(limit, run).await {
            Ok(result) => (result, false),
            Err(_) => (Err(Error::Offline), true),
        };
        let end = policy::end_after(&spent, &result, timed_out);
        // A message that made it into the outbox goes later whatever happened
        // here, so the app gets its id, not an error that invites a retype.
        let queued =
            sending && result.is_err() && client.cache().outbox().is_ok_and(|o| o.iter().any(|o| o.msg_id == new.id));
        let result = if queued { Ok(Reply::Sent(new.id)) } else { result };
        let synced_now = policy::syncs_after(was_pending, &spent, &result);
        let _ = reply.send(result);
        if end.is_none() && synced_now {
            // Active now, so it can sync.
            return first_sync(client).await.err().map(End::from);
        }
        end
    }
}

/// The time now, in milliseconds since the epoch.
fn now_ms() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_millis() as u64)
}
