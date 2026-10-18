//! The driver: a tokio task that keeps the client connected. It unlocks the
//! key file offline, opens the saved chats, then connects, resumes and syncs,
//! reconnecting with back-off (Decision 16) whenever the connection drops.
//! The app talks to it through commands and hears back through updates. A
//! sign-out stops it for good and deletes the saved chats.

use std::future::Future;
use std::path::{Path, PathBuf};
use std::time::Duration;

use corvo_chat_proto::frames::{Account, Device, SignedOutReason};
use tokio::sync::{mpsc, oneshot};
use zeroize::Zeroizing;

use super::commands::{self, Command, KeyFile, Live, NewMessage, Reply};
use super::{NativeTransport, Roots, ws::CONNECT_TIMEOUT};
use crate::cache::{Cache, SqliteCache};
use crate::client::Client;
use crate::sync::{ConnState, Update};
use crate::{Error, LocalKey, Secrets, SignedIn, Strength, TransportError, hello, resume};

/// The longest wait between tries.
const MAX_BACKOFF: u64 = 30;
/// How long one command may take before the connection is given up: a
/// reply that never comes means the connection is no good.
const COMMAND_TIMEOUT: Duration = Duration::from_secs(30);
/// The same for a command that proves the password again, which runs the
/// slow password hash here before the server can answer.
const PASSWORD_TIMEOUT: Duration = Duration::from_secs(120);

type Call = (Command, oneshot::Sender<Result<Reply, Error>>);

/// The handle to a running driver. Dropping it stops the driver.
pub struct Driver {
    calls: mpsc::UnboundedSender<Call>,
}

/// The key file for an account just signed in (signed up, signed in, or
/// recovered), locked under `password`. Writes nothing: the app saves
/// `to_json()` readable by its owner only.
pub fn first_start(url: &str, signed_in: &SignedIn, password: &str, device: &Device, s: Strength) -> LocalKey {
    LocalKey::lock(url, &signed_in.account.username, device.id.0, password, &signed_in.secrets, &signed_in.token, s)
}

impl Driver {
    /// Unlocks the key file (no network needed), opens the saved chats at
    /// `cache_path`, and starts the driver on the current tokio runtime. A
    /// wrong password or another account's cache fails here. `app_version`
    /// is what `hello` tells the server.
    #[allow(clippy::too_many_arguments)] // each is a separate thing the app holds
    pub fn start(
        url: String,
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
        let (reply, answer) = oneshot::channel();
        self.calls.send((command, reply)).map_err(|_| Error::Stopped)?;
        answer.await.map_err(|_| Error::Stopped)?
    }
}

/// Why a connection ended.
enum End {
    /// Dropped or broken: try again after the back-off.
    Lost,
    /// Signed out, for good.
    SignedOut(SignedOutReason),
    /// The server needs a newer app: trying again won't help.
    UpdateRequired,
    /// The app dropped the driver.
    Shutdown,
}

impl From<Error> for End {
    fn from(e: Error) -> End {
        match e {
            Error::SignedOut(reason) => End::SignedOut(reason),
            Error::UpdateRequired => End::UpdateRequired,
            _ => End::Lost,
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
            match end {
                End::Shutdown => return,
                End::UpdateRequired => {
                    // Told once; what's saved can still be read until the app quits.
                    self.tell(Update::Connection(ConnState::UpdateRequired));
                    self.offline_until(&mut cache, std::future::pending::<()>()).await;
                    return;
                }
                End::SignedOut(reason) => {
                    // Closed before its files go.
                    drop(cache);
                    delete_cache(&self.path);
                    self.tell(Update::SignedOut(reason));
                    return;
                }
                End::Lost => {
                    self.tell(Update::Connection(ConnState::Reconnecting { in_secs: backoff }));
                    let wait = tokio::time::sleep(Duration::from_secs(backoff));
                    if self.offline_until(&mut cache, wait).await.is_none() {
                        return;
                    }
                    backoff = (backoff * 2).min(MAX_BACKOFF);
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
                    let _ = reply.send(commands::offline(cache, &self.secrets, command));
                }
            }
        }
    }

    /// Syncs a new connection, then runs it until it ends. A pending account
    /// can't sync, so it waits for `Activate` first.
    async fn live(&mut self, client: &mut Live, backoff: &mut u64) -> End {
        if client.account().pending_until.is_none()
            && let Err(e) = client.sync().await
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
        let after = match command {
            Command::SignOut => Some(SignedOutReason::ThisDevice),
            Command::DeleteAccount { .. } => Some(SignedOutReason::AccountDeleted),
            _ => None,
        };
        let activating = matches!(command, Command::Activate { .. });
        let sending = matches!(command, Command::SendText { .. });
        let limit = match command {
            Command::ChangePassword { .. } | Command::NewRecoveryCode { .. } | Command::DeleteAccount { .. } => {
                PASSWORD_TIMEOUT
            }
            _ => COMMAND_TIMEOUT,
        };
        let new = NewMessage::now();
        let run = commands::online(client, command, &self.key_file, &new);
        let (result, timed_out) = match tokio::time::timeout(limit, run).await {
            Ok(result) => (result, false),
            Err(_) => (Err(Error::Offline), true),
        };
        let end = match &result {
            _ if timed_out => Some(End::Lost),
            Ok(_) => after.map(End::SignedOut),
            Err(Error::SignedOut(reason)) => Some(End::SignedOut(*reason)),
            Err(Error::Transport(_) | Error::BadServerMessage | Error::Cache(_)) => Some(End::Lost),
            Err(_) => None,
        };
        // A message that made it into the outbox goes later whatever happened
        // here, so the app gets its id, not an error that invites a retype.
        let queued =
            sending && result.is_err() && client.cache().outbox().is_ok_and(|o| o.iter().any(|o| o.msg_id == new.id));
        let result = if queued { Ok(Reply::Sent(new.id)) } else { result };
        let synced_now = activating && result.is_ok();
        let _ = reply.send(result);
        if end.is_none() && synced_now {
            // Active now, so it can sync.
            return client.sync().await.err().map(End::from);
        }
        end
    }
}
