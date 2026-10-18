mod common;

use common::{PASSWORD, S, TestClient, device, hello_conn};
use corvo_chat_core::{Client, Error, MemoryCache, Recovery, sign_in, sign_up};
use corvo_chat_proto::{
    Number,
    frames::{ErrorCode, SignedOutReason},
};

const NEW_PASSWORD: &str = "a brand new password";

/// An account with its username, number and first recovery code.
struct User {
    client: TestClient,
    username: String,
    number: Number,
    code: String,
}

/// Signs up a new account, still pending.
async fn pending(server: &common::TestServer, base: &str) -> User {
    let mut t = hello_conn(server).await;
    let dev = device("laptop");
    let up = sign_up(&mut t, base, "Abin", PASSWORD, &dev, S).await.unwrap();
    let (username, number) = (up.signed_in.account.username.clone(), up.signed_in.account.number);
    let code = up.recovery_code.display();
    User { client: Client::new(t, up.signed_in, dev, S, MemoryCache::default()), username, number, code }
}

/// Signs up a new account and activates it as the admin would.
async fn active(server: &common::TestServer, base: &str) -> User {
    let user = pending(server, base).await;
    common::activate(server, user.number);
    user
}

/// Signs this account in on another device.
async fn second_device(server: &common::TestServer, username: &str, password: &str) -> TestClient {
    let mut t = hello_conn(server).await;
    let dev = device("phone");
    let signed_in = sign_in(&mut t, username, password, &dev, S).await.unwrap();
    Client::new(t, signed_in, dev, S, MemoryCache::default())
}

#[tokio::test]
async fn invite_then_activate_makes_the_account_active() {
    let server = common::start().await;
    let mut a = active(&server, "alice").await;
    let mut b = pending(&server, "bob").await;
    assert!(b.client.account().pending_until.is_some());

    let invite = a.client.invite(b.number).await.unwrap();
    assert_eq!(invite.number, b.number);
    assert_eq!(a.client.invites().await.unwrap(), vec![invite.clone()]);

    let account = b.client.activate(&invite.code).await.unwrap();
    assert!(account.pending_until.is_none());
    assert!(b.client.account().pending_until.is_none());
    // An active account can use the active-only frames.
    assert_eq!(b.client.devices().await.unwrap().len(), 1);
}

#[tokio::test]
async fn a_wrong_activation_code_is_an_error_and_the_client_goes_on() {
    let server = common::start().await;
    let mut b = pending(&server, "bob").await;
    let result = b.client.activate("000000").await;
    assert!(matches!(result, Err(Error::Server { .. })));
    assert!(b.client.account().pending_until.is_some());
}

#[tokio::test]
async fn change_password_signs_out_the_other_device() {
    let server = common::start().await;
    let mut one = active(&server, "alice").await;
    let mut two = second_device(&server, &one.username, PASSWORD).await;

    one.client.change_password(PASSWORD, NEW_PASSWORD).await.unwrap();
    assert!(matches!(two.devices().await, Err(Error::SignedOut(SignedOutReason::PasswordChanged))));
    assert_eq!(one.client.devices().await.unwrap().len(), 1);

    let mut t = hello_conn(&server).await;
    let old = sign_in(&mut t, &one.username, PASSWORD, &device("x"), S).await;
    assert!(matches!(old, Err(Error::WrongCredentials)));
    let fresh = sign_in(&mut t, &one.username, NEW_PASSWORD, &device("x"), S).await.unwrap();
    // The re-locked copy opens to the same secrets.
    assert_eq!(fresh.secrets.public_keys(), one.client.secrets().public_keys());
}

#[tokio::test]
async fn a_weak_new_password_is_refused_before_anything_is_sent() {
    let server = common::start().await;
    let mut one = active(&server, "alice").await;
    assert!(matches!(one.client.change_password(PASSWORD, "short").await, Err(Error::PasswordTooShort)));
    // Nothing was started, so the connection still answers in step.
    assert_eq!(one.client.devices().await.unwrap().len(), 1);
}

#[tokio::test]
async fn wrong_old_password_changes_nothing() {
    let server = common::start().await;
    let mut one = active(&server, "alice").await;
    let result = one.client.change_password("not the password", NEW_PASSWORD).await;
    assert!(matches!(result, Err(Error::WrongCredentials)));

    let mut t = hello_conn(&server).await;
    assert!(sign_in(&mut t, &one.username, PASSWORD, &device("x"), S).await.is_ok());
    let mut t = hello_conn(&server).await;
    assert!(matches!(
        sign_in(&mut t, &one.username, NEW_PASSWORD, &device("x"), S).await,
        Err(Error::WrongCredentials)
    ));
}

#[tokio::test]
async fn new_recovery_code_replaces_the_old_one() {
    let server = common::start().await;
    let mut one = active(&server, "alice").await;
    let mut two = second_device(&server, &one.username, PASSWORD).await;

    let code = one.client.new_recovery_code(PASSWORD).await.unwrap();
    assert_ne!(code.display(), one.code);
    // Other devices stay signed in.
    assert_eq!(two.devices().await.unwrap().len(), 2);

    let mut t = hello_conn(&server).await;
    let old = Recovery::start(&mut t, &one.username, &one.code, &device("x")).await;
    assert!(matches!(old, Err(Error::WrongCredentials)));
    let mut t = hello_conn(&server).await;
    assert!(Recovery::start(&mut t, &one.username, &code.display(), &device("x")).await.is_ok());
}

#[tokio::test]
async fn devices_list_and_sign_out_another() {
    let server = common::start().await;
    let mut one = active(&server, "alice").await;
    let mut two = second_device(&server, &one.username, PASSWORD).await;

    let devices = one.client.devices().await.unwrap();
    assert_eq!(devices.len(), 2);
    assert_eq!(devices.iter().filter(|d| d.this_device).count(), 1);
    assert_eq!(devices.iter().find(|d| d.this_device).unwrap().name, "laptop");
    let other = devices.iter().find(|d| !d.this_device).unwrap().id.0;

    let left = one.client.sign_out_device(other).await.unwrap();
    assert_eq!(left.len(), 1);
    assert!(matches!(two.devices().await, Err(Error::SignedOut(SignedOutReason::ByAnotherDevice))));
}

#[tokio::test]
async fn sign_out_ends_this_device() {
    let server = common::start().await;
    let mut one = active(&server, "alice").await;
    one.client.sign_out().await.unwrap();
}

#[tokio::test]
async fn delete_account_signs_out() {
    let server = common::start().await;
    let mut one = active(&server, "alice").await;
    let mut two = second_device(&server, &one.username, PASSWORD).await;

    one.client.delete_account(PASSWORD).await.unwrap();
    assert!(matches!(two.devices().await, Err(Error::SignedOut(SignedOutReason::AccountDeleted))));
    let mut t = hello_conn(&server).await;
    assert!(matches!(sign_in(&mut t, &one.username, PASSWORD, &device("x"), S).await, Err(Error::WrongCredentials)));
}

#[tokio::test]
async fn a_wrong_password_does_not_delete_the_account() {
    let server = common::start().await;
    let mut one = active(&server, "alice").await;
    assert!(matches!(one.client.delete_account("not the password").await, Err(Error::WrongCredentials)));
    assert_eq!(one.client.devices().await.unwrap().len(), 1);
}

#[tokio::test]
async fn names_change_and_a_bad_one_is_an_error() {
    let server = common::start().await;
    let mut one = active(&server, "alice").await;

    one.client.set_display_name("  Alice B  ").await.unwrap();
    assert_eq!(one.client.account().display_name, "Alice B");
    one.client.rename_device("work laptop").await.unwrap();
    let devices = one.client.devices().await.unwrap();
    assert_eq!(devices[0].name, "work laptop");

    let result = one.client.set_display_name("").await;
    assert!(matches!(result, Err(Error::Server { code: ErrorCode::BadDisplayName, .. })));
    assert_eq!(one.client.account().display_name, "Alice B");
    // The refused frame left nothing behind to confuse the next reply.
    assert_eq!(one.client.devices().await.unwrap().len(), 1);
}
