mod common;

use corvo_chat_core::{
    Error, Recovery, Strength, Transport, TransportError, hello, native::NativeTransport, resume, sign_in, sign_up,
};
use corvo_chat_proto::{
    B64, Number,
    frames::{ClientFrame, Device, DeviceKind, ServerFrame, SignedOutReason},
};
use corvo_chat_server::accounts;
use rand_core::{OsRng, RngCore};

const S: Strength = Strength::FastInsecure;
const PASSWORD: &str = "correct horse battery";

/// Wraps a transport and keeps every frame sent through it.
struct Recorder {
    inner: NativeTransport,
    sent: Vec<ClientFrame>,
}

impl Transport for Recorder {
    async fn send(&mut self, frame: &ClientFrame) -> Result<(), TransportError> {
        self.sent.push(frame.clone());
        self.inner.send(frame).await
    }

    async fn recv(&mut self) -> Result<ServerFrame, TransportError> {
        self.inner.recv().await
    }
}

fn device() -> Device {
    let mut id = [0u8; 16];
    OsRng.fill_bytes(&mut id);
    Device { id: B64(id), kind: DeviceKind::Cli, name: "test laptop".into() }
}

/// A connection that has said hello.
async fn hello_conn(server: &common::TestServer) -> NativeTransport {
    let mut t = common::connect(server).await;
    hello(&mut t, "0.0.0").await.expect("the server is ready");
    t
}

#[tokio::test]
async fn sign_up_gives_a_username_number_and_recovery_code() {
    let server = common::start().await;
    let mut t = hello_conn(&server).await;
    let up = sign_up(&mut t, "abin", "Abin", PASSWORD, &device(), S).await.unwrap();
    let name = &up.signed_in.account.username;
    let (base, end) = name.split_once('-').expect("a dash");
    assert_eq!(base, "abin");
    assert_eq!(end.len(), 4);
    assert!(up.signed_in.account.pending_until.is_some());
    let digits = up.signed_in.account.number.digits();
    assert_eq!(digits.len(), 10);
    assert!(Number::parse(&digits).is_ok());
    assert_eq!(up.recovery_code.display().split('-').count(), 6);
}

#[tokio::test]
async fn sign_in_on_a_second_device_unlocks_the_same_secrets() {
    let server = common::start().await;
    let mut t = hello_conn(&server).await;
    let up = sign_up(&mut t, "abin", "Abin", PASSWORD, &device(), S).await.unwrap();

    let mut t2 = hello_conn(&server).await;
    let again = sign_in(&mut t2, &up.signed_in.account.username, PASSWORD, &device(), S).await.unwrap();
    assert_eq!(again.secrets.public_keys(), up.signed_in.secrets.public_keys());
    assert_eq!(again.account, up.signed_in.account);
}

#[tokio::test]
async fn resume_with_the_token_works_and_a_signed_out_token_does_not() {
    let server = common::start().await;
    let mut t = hello_conn(&server).await;
    let dev = device();
    let up = sign_up(&mut t, "abin", "Abin", PASSWORD, &dev, S).await.unwrap();

    let mut t2 = hello_conn(&server).await;
    let account = resume(&mut t2, &up.signed_in.token, &dev.id.0).await.unwrap();
    assert_eq!(account, up.signed_in.account);

    t2.send(&ClientFrame::SignOut).await.unwrap();
    assert!(matches!(t2.recv().await.unwrap(), ServerFrame::SignedOut { reason: SignedOutReason::ThisDevice }));

    let mut t3 = hello_conn(&server).await;
    let gone = resume(&mut t3, &up.signed_in.token, &dev.id.0).await;
    assert!(matches!(gone, Err(Error::SignedOut(SignedOutReason::ThisDevice))));
}

#[tokio::test]
async fn wrong_password_and_unknown_user_look_the_same() {
    let server = common::start().await;
    let mut t = hello_conn(&server).await;
    let up = sign_up(&mut t, "abin", "Abin", PASSWORD, &device(), S).await.unwrap();
    let username = up.signed_in.account.username.clone();

    let mut r = Recorder { inner: common::connect(&server).await, sent: vec![] };
    hello(&mut r, "0.0.0").await.unwrap();
    let wrong = sign_in(&mut r, &username, "not the password!", &device(), S).await;
    assert!(matches!(wrong, Err(Error::WrongCredentials)));
    let unknown = sign_in(&mut r, "nobody-abcd", PASSWORD, &device(), S).await;
    assert!(matches!(unknown, Err(Error::WrongCredentials)));

    let finishes = r.sent.iter().filter(|f| matches!(f, ClientFrame::LoginFinish { .. })).count();
    assert_eq!(finishes, 0);
    let starts = r.sent.iter().filter(|f| matches!(f, ClientFrame::LoginStart { .. })).count();
    assert_eq!(starts, 2);

    let now = server.state.clock.now();
    let count = server
        .state
        .db
        .call(move |c| accounts::by_username(c, &username, now).unwrap().expect("the account").wrong_count)
        .await;
    assert_eq!(count, 1);
}

#[tokio::test]
async fn weak_password_is_refused_before_the_server_sees_it() {
    let server = common::start().await;
    let mut r = Recorder { inner: common::connect(&server).await, sent: vec![] };
    let result = sign_up(&mut r, "abin", "Abin", "short", &device(), S).await;
    assert!(matches!(result, Err(Error::PasswordTooShort)));
    assert!(r.sent.is_empty());
}

#[tokio::test]
async fn recovery_signs_in_with_a_new_password_and_code() {
    let server = common::start().await;
    let mut t = hello_conn(&server).await;
    let up = sign_up(&mut t, "abin", "Abin", PASSWORD, &device(), S).await.unwrap();
    let username = up.signed_in.account.username.clone();
    let old_code = up.recovery_code.display();

    let mut t2 = hello_conn(&server).await;
    let mut recovery = Recovery::start(&mut t2, &username, &old_code.to_lowercase(), &device()).await.unwrap();
    assert!(matches!(recovery.complete("short", S).await, Err(Error::PasswordTooShort)));
    let new = recovery.complete("a brand new password", S).await.unwrap();
    assert!(matches!(recovery.complete("another new password", S).await, Err(Error::BadServerMessage)));
    assert_eq!(new.signed_in.secrets.public_keys(), up.signed_in.secrets.public_keys());
    assert_ne!(new.recovery_code.display(), old_code);

    let mut t3 = hello_conn(&server).await;
    let old = sign_in(&mut t3, &username, PASSWORD, &device(), S).await;
    assert!(matches!(old, Err(Error::WrongCredentials)));
    let fresh = sign_in(&mut t3, &username, "a brand new password", &device(), S).await.unwrap();
    assert_eq!(fresh.secrets.public_keys(), up.signed_in.secrets.public_keys());

    let mut r = Recorder { inner: common::connect(&server).await, sent: vec![] };
    hello(&mut r, "0.0.0").await.unwrap();
    let stale = Recovery::start(&mut r, &username, &old_code, &device()).await;
    assert!(matches!(stale, Err(Error::WrongCredentials)));
    assert!(!r.sent.iter().any(|f| matches!(f, ClientFrame::RecoveryFinish { .. })));
    let current = Recovery::start(&mut r, &username, &new.recovery_code.display(), &device()).await;
    assert!(current.is_ok());
}

#[tokio::test]
async fn resuming_an_expired_pending_account_is_signed_out() {
    let server = common::start().await;
    let mut t = hello_conn(&server).await;
    let dev = device();
    let up = sign_up(&mut t, "abin", "Abin", PASSWORD, &dev, S).await.unwrap();
    let number = up.signed_in.account.number.digits();
    server
        .state
        .db
        .call(move |c| c.execute("UPDATE accounts SET pending_until = 1 WHERE number = ?1", [number]).unwrap())
        .await;

    let mut t2 = hello_conn(&server).await;
    let result = resume(&mut t2, &up.signed_in.token, &dev.id.0).await;
    assert!(matches!(result, Err(Error::SignedOut(SignedOutReason::Expired))));
}
