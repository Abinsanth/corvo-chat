//! Hello, sign up, sign in, resume and recovery: the first frames a client
//! sends, in the order protocol §5 gives.

use corvo_chat_proto::{
    B64, PROTOCOL_VERSION,
    frames::{Account, ClientFrame, Device, ErrorCode, ServerFrame, SignedOutReason},
    opaque::recovery_ksf,
};
use zeroize::Zeroizing;

use crate::crypto::{
    CopyKind, RecoveryCode, Secrets, check_password, lock_copy, normalize_password, opaque, unlock_copy,
};
use crate::{Error, Strength, Transport};

/// A signed-in device: its session token, the account, and the unlocked secrets.
pub struct SignedIn {
    pub token: Zeroizing<[u8; 32]>,
    pub account: Account,
    pub secrets: Secrets,
}

impl std::fmt::Debug for SignedIn {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SignedIn").field("account", &self.account).finish_non_exhaustive()
    }
}

/// A new account (or a recovered one) with the recovery code to show once.
#[derive(Debug)]
pub struct SignedUp {
    pub signed_in: SignedIn,
    pub recovery_code: RecoveryCode,
}

/// The next frame, with the server's errors and sign-outs turned into `Err`.
async fn next<T: Transport>(t: &mut T) -> Result<ServerFrame, Error> {
    match t.recv().await? {
        ServerFrame::Error { code: ErrorCode::UpdateRequired, .. } => Err(Error::UpdateRequired),
        ServerFrame::Error { code: ErrorCode::WrongCredentials, .. } => Err(Error::WrongCredentials),
        ServerFrame::Error { code, message, until } => Err(Error::Server { code, message, until }),
        ServerFrame::SignedOut { reason } => Err(Error::SignedOut(reason)),
        frame => Ok(frame),
    }
}

/// Says hello. Gives the server's version.
pub async fn hello<T: Transport>(t: &mut T, app_version: &str) -> Result<String, Error> {
    t.send(&ClientFrame::Hello { protocol_version: PROTOCOL_VERSION, app_version: app_version.into() }).await?;
    match next(t).await? {
        ServerFrame::Ready { server_version } => Ok(server_version),
        _ => Err(Error::BadServerMessage),
    }
}

/// The signed-in frame of a flow that gives no password copy.
fn signed_in_frame(frame: ServerFrame) -> Result<(Zeroizing<[u8; 32]>, Account), Error> {
    match frame {
        ServerFrame::SignedIn { token, account, .. } => Ok((Zeroizing::new(token.0), account)),
        _ => Err(Error::BadServerMessage),
    }
}

/// What `signup_finish` and `recovery_complete` carry.
struct Registered {
    password: corvo_chat_proto::opaque::RegUpload,
    recovery: corvo_chat_proto::opaque::RegUpload,
    password_copy: corvo_chat_proto::Blob,
    recovery_copy: corvo_chat_proto::Blob,
}

/// Finishes both registrations and locks `secrets` under both new wrap keys.
fn register_both(
    started: (opaque::Registering, opaque::Registering),
    password: &str,
    code: &RecoveryCode,
    responses: (&corvo_chat_proto::opaque::RegResponse, &corvo_chat_proto::opaque::RegResponse),
    secrets: &Secrets,
    s: Strength,
) -> Result<Registered, Error> {
    let (password_upload, password_key) =
        opaque::register_finish(started.0, password.as_bytes(), responses.0, &opaque::password_ksf(s))?;
    let (recovery_upload, recovery_key) =
        opaque::register_finish(started.1, code.secret(), responses.1, &recovery_ksf())?;
    Ok(Registered {
        password: password_upload,
        recovery: recovery_upload,
        password_copy: lock_copy(secrets, &password_key, CopyKind::Password),
        recovery_copy: lock_copy(secrets, &recovery_key, CopyKind::Recovery),
    })
}

/// Makes an account. The password is checked before anything is sent. If the
/// server says someone took the name in the meantime (`try_again`), this
/// starts over once by itself.
pub async fn sign_up<T: Transport>(
    t: &mut T,
    base: &str,
    display_name: &str,
    password: &str,
    device: &Device,
    s: Strength,
) -> Result<SignedUp, Error> {
    let password = normalize_password(password);
    check_password(&password)?;
    let mut tries = 0;
    loop {
        tries += 1;
        match sign_up_once(t, base, display_name, &password, device, s).await {
            Err(Error::Server { code: ErrorCode::TryAgain, .. }) if tries < 2 => {}
            other => return other,
        }
    }
}

/// One try at the sign-up frames, from `signup_start` to `signed_in`.
async fn sign_up_once<T: Transport>(
    t: &mut T,
    base: &str,
    display_name: &str,
    password: &str,
    device: &Device,
    s: Strength,
) -> Result<SignedUp, Error> {
    let code = RecoveryCode::generate();
    let (password_state, password_request) = opaque::register_start(password.as_bytes());
    let (recovery_state, recovery_request) = opaque::register_start(code.secret());
    t.send(&ClientFrame::SignupStart {
        base: base.into(),
        display_name: display_name.into(),
        device: device.clone(),
        password: password_request,
        recovery: recovery_request,
    })
    .await?;
    let (password_response, recovery_response) = match next(t).await? {
        ServerFrame::SignupOffer { password, recovery, .. } => (password, recovery),
        _ => return Err(Error::BadServerMessage),
    };

    let secrets = Secrets::generate();
    let done = register_both(
        (password_state, recovery_state),
        password,
        &code,
        (&password_response, &recovery_response),
        &secrets,
        s,
    )?;
    t.send(&ClientFrame::SignupFinish {
        password: done.password,
        recovery: done.recovery,
        password_copy: done.password_copy,
        recovery_copy: done.recovery_copy,
        public_keys: secrets.public_keys(),
    })
    .await?;
    let (token, account) = signed_in_frame(next(t).await?)?;
    Ok(SignedUp { signed_in: SignedIn { token, account, secrets }, recovery_code: code })
}

/// Signs in with the password. A wrong password (or an unknown name) fails at
/// the client's OPAQUE finish, and then nothing more is sent.
pub async fn sign_in<T: Transport>(
    t: &mut T,
    username: &str,
    password: &str,
    device: &Device,
    s: Strength,
) -> Result<SignedIn, Error> {
    let password = normalize_password(password);
    let (state, request) = opaque::login_start(password.as_bytes());
    t.send(&ClientFrame::LoginStart { username: username.into(), request, device: device.clone() }).await?;
    let response = match next(t).await? {
        ServerFrame::LoginOffer { response } => response,
        _ => return Err(Error::BadServerMessage),
    };
    let (finalization, wrap_key) =
        opaque::login_finish(state, password.as_bytes(), &response, &opaque::password_ksf(s))?;
    t.send(&ClientFrame::LoginFinish { finalization }).await?;
    match next(t).await? {
        ServerFrame::SignedIn { token, account, copy: Some(copy) } => {
            let secrets = unlock_copy(&copy, &wrap_key, CopyKind::Password).ok_or(Error::BadServerMessage)?;
            Ok(SignedIn { token: Zeroizing::new(token.0), account, secrets })
        }
        _ => Err(Error::BadServerMessage),
    }
}

/// Signs in again with the token this device kept.
pub async fn resume<T: Transport>(t: &mut T, token: &[u8; 32], device_id: &[u8; 16]) -> Result<Account, Error> {
    t.send(&ClientFrame::Resume { token: B64(*token), device_id: B64(*device_id) }).await?;
    match next(t).await {
        Err(Error::Server { code: ErrorCode::SessionUnknown, .. }) => {
            Err(Error::SignedOut(SignedOutReason::ThisDevice))
        }
        // The server follows this error with `signed_out {expired}`.
        Err(Error::Server { code: ErrorCode::SignupExpired, .. }) => Err(Error::SignedOut(SignedOutReason::Expired)),
        Err(e) => Err(e),
        Ok(frame) => signed_in_frame(frame).map(|(_, account)| account),
    }
}

/// A recovery under way: the recovery code is proven and the secrets are
/// unlocked, waiting for a new password. Nothing changes on the server until
/// [`Recovery::complete`].
pub struct Recovery<'t, T> {
    t: &'t mut T,
    /// Taken once the server signs this device in.
    secrets: Option<Secrets>,
}

impl<'t, T: Transport> Recovery<'t, T> {
    /// Proves the recovery code and unlocks the recovery copy. A wrong code
    /// is `WrongCredentials`, and nothing more is sent.
    pub async fn start(t: &'t mut T, username: &str, code: &str, device: &Device) -> Result<Recovery<'t, T>, Error> {
        let code = RecoveryCode::parse(code)?;
        let (state, request) = opaque::login_start(code.secret());
        t.send(&ClientFrame::RecoveryStart { username: username.into(), request, device: device.clone() }).await?;
        let response = match next(t).await? {
            ServerFrame::LoginOffer { response } => response,
            _ => return Err(Error::BadServerMessage),
        };
        let (finalization, wrap_key) = opaque::login_finish(state, code.secret(), &response, &recovery_ksf())?;
        t.send(&ClientFrame::RecoveryFinish { finalization }).await?;
        match next(t).await? {
            ServerFrame::RecoveryOffer { recovery_copy } => {
                let secrets =
                    unlock_copy(&recovery_copy, &wrap_key, CopyKind::Recovery).ok_or(Error::BadServerMessage)?;
                Ok(Recovery { t, secrets: Some(secrets) })
            }
            _ => Err(Error::BadServerMessage),
        }
    }

    /// Sets the new password and a new recovery code, and signs this device in.
    /// Every other device is signed out. After a failure (a weak password, say)
    /// it can be called again; after a success it can't.
    pub async fn complete(&mut self, new_password: &str, s: Strength) -> Result<SignedUp, Error> {
        let Recovery { t, secrets } = self;
        let t: &mut T = t;
        let secrets_ref = secrets.as_ref().ok_or(Error::BadServerMessage)?;
        let password = normalize_password(new_password);
        check_password(&password)?;
        let code = RecoveryCode::generate();
        let (password_state, password_request) = opaque::register_start(password.as_bytes());
        let (recovery_state, recovery_request) = opaque::register_start(code.secret());
        t.send(&ClientFrame::Reregister { password: Some(password_request), recovery: Some(recovery_request) }).await?;
        let (password_response, recovery_response) = match next(t).await? {
            ServerFrame::Registration { password: Some(p), recovery: Some(r) } => (p, r),
            _ => return Err(Error::BadServerMessage),
        };
        let done = register_both(
            (password_state, recovery_state),
            &password,
            &code,
            (&password_response, &recovery_response),
            secrets_ref,
            s,
        )?;
        t.send(&ClientFrame::RecoveryComplete {
            password: done.password,
            recovery: done.recovery,
            password_copy: done.password_copy,
            recovery_copy: done.recovery_copy,
        })
        .await?;
        let (token, account) = signed_in_frame(next(t).await?)?;
        let secrets = secrets.take().ok_or(Error::BadServerMessage)?;
        Ok(SignedUp { signed_in: SignedIn { token, account, secrets }, recovery_code: code })
    }
}
