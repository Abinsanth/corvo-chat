//! The Profile tab: your display name, number and username, and a menu for
//! your account, each item a window with its own fields (see `form`). And
//! what happens when this device is signed out.

use corvo_chat_core::native::Command;
use corvo_chat_proto::frames::SignedOutReason;

use super::form::{self, ACCOUNT_DELETED, Form};
use super::start::signed_out_note;
use super::state::{Line, State, Tone};
use super::update::Effect;
use super::window::Overlay;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MenuItem {
    Name,
    Number,
    Username,
    Password,
    RecoveryCode,
    Devices,
    DeleteAccount,
}

impl MenuItem {
    /// Whether Enter opens something: your number and username are only shown.
    fn opens(self) -> bool {
        !matches!(self, MenuItem::Number | MenuItem::Username)
    }
}

/// The menu, in the spec's order.
pub const MENU: &[(&str, MenuItem)] = &[
    ("Display name", MenuItem::Name),
    ("Your number", MenuItem::Number),
    ("Your username", MenuItem::Username),
    ("Change password", MenuItem::Password),
    ("New recovery code", MenuItem::RecoveryCode),
    ("Devices", MenuItem::Devices),
    ("Delete account", MenuItem::DeleteAccount),
];

const HINT: &str = "↑/↓ choose · Enter opens · Esc closes the tab";
/// The labels' column, so the values line up.
const LABEL: usize = 15;

/// What the Profile tab shows: the menu, the chosen row marked `›` (your
/// number and username can't be chosen), Delete account in red, then how
/// to use it.
pub fn lines(state: &State) -> Vec<Line> {
    let mut lines = Vec::new();
    for (i, (label, item)) in MENU.iter().enumerate() {
        let mark = if i == state.profile_at { '›' } else { ' ' };
        let value = match item {
            MenuItem::Name => Some(&state.name),
            MenuItem::Number => Some(&state.number),
            MenuItem::Username => Some(&state.username),
            _ => None,
        };
        let text = match value {
            Some(value) => format!("{mark} {label:<LABEL$}{}", value.replace('\n', " ")),
            None => format!("{mark} {label}"),
        };
        let tone = if *item == MenuItem::DeleteAccount { Tone::Bad } else { Tone::Plain };
        lines.push(Line { text, tone });
    }
    lines.push(Line { text: String::new(), tone: Tone::Plain });
    lines.push(Line { text: HINT.into(), tone: Tone::Dim });
    lines
}

/// Moves the `›` by `delta` to the next row that opens something, staying
/// on the menu.
pub fn step(state: &mut State, delta: isize) {
    let mut at = state.profile_at as isize;
    loop {
        at += delta.signum();
        let Some((_, item)) = usize::try_from(at).ok().and_then(|i| MENU.get(i)) else { return };
        if item.opens() {
            state.profile_at = at as usize;
            return;
        }
    }
}

/// Opens the window for the row marked `›`.
pub fn open(state: &mut State, effects: &mut Vec<Effect>) {
    let form = match MENU[state.profile_at].1 {
        MenuItem::Name => Form::name(&state.name),
        MenuItem::Password => Form::password(),
        MenuItem::RecoveryCode => Form::recovery_code(),
        MenuItem::Devices => {
            effects.push(Effect::Call(Command::Devices));
            Form::devices()
        }
        MenuItem::DeleteAccount => Form::delete_account(),
        MenuItem::Number | MenuItem::Username => return,
    };
    state.overlay = Some(Overlay::form(form));
}

/// This device was signed out (Decision 7): back to Sign in / Sign up,
/// saying why. If the account is gone because Delete account asked for it,
/// that's done: the app leaves.
pub fn signed_out(state: &State, reason: SignedOutReason) -> Effect {
    if reason == SignedOutReason::AccountDeleted && form::deleting(state) {
        return Effect::Leave(vec![ACCOUNT_DELETED.into()]);
    }
    Effect::SignedOut(signed_out_note(reason).into())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::files::{CACHE_FILE, DEVICE_ID_FILE, KEY_FILE, USERNAME_FILE};
    use crate::ui::form::{DELETE_WARNING, DeleteStep, FormKind, PASSWORD_CHANGED};
    use crate::ui::runner::testing::{App, PASSWORD, Proxy, TestServer};
    use crate::ui::start::Start;
    use crate::ui::state::{Screen, TabKind};
    use crate::ui::update::{Msg, update};
    use crate::ui::view::render;
    use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    use ratatui::style::Color;
    use std::path::Path;

    const NEW_PASSWORD: &str = "a brand new password";

    fn form(state: &State) -> &Form {
        match &state.overlay {
            Some(Overlay::Form(form)) => form,
            other => panic!("no form: {other:?}"),
        }
    }

    fn note(state: &State) -> String {
        form(state).note.as_ref().map(|n| n.text.clone()).unwrap_or_default()
    }

    fn label(state: &State) -> String {
        state.ask().expect("a question").label
    }

    /// The start screen's notes.
    fn notes(state: &State) -> Vec<String> {
        match &state.screen {
            Screen::Start(start) => start.notes.iter().map(|n| n.text.clone()).collect(),
            Screen::Main => panic!("still on the main screen"),
        }
    }

    fn at_start(state: &State) -> bool {
        matches!(state.screen, Screen::Start(_)) && state.ask().is_some_and(|a| a.label == "s, u or r: ")
    }

    fn screen_of(state: &State, w: u16, h: u16) -> String {
        let buf = render(state, w, h);
        (0..h).map(|y| (0..w).map(|x| buf[(x, y)].symbol()).collect::<String>()).collect::<Vec<_>>().join("\n")
    }

    /// The recovery code on screen: six groups of four, joined by dashes.
    fn code_on(screen: &str) -> String {
        screen
            .split_whitespace()
            .find(|w| w.len() == 29 && w.matches('-').count() == 5)
            .unwrap_or_else(|| panic!("no recovery code on screen:\n{screen}"))
            .to_string()
    }

    /// Opens the Profile tab and the window for `item`.
    async fn open_item(app: &mut App, item: MenuItem) {
        app.send("/profile").await;
        let target = MENU.iter().position(|(_, i)| *i == item).expect("on the menu");
        while app.state.profile_at < target {
            app.press(KeyCode::Down).await;
        }
        app.press(KeyCode::Enter).await;
    }

    /// Signs in on the data folder `dir` as a second device of `username`.
    async fn second_device(dir: &Path, server: &TestServer, username: &str) -> App {
        let mut app = App::open(dir, server);
        for answer in ["s", username, PASSWORD] {
            app.send(answer).await;
        }
        assert!(matches!(app.state.screen, Screen::Main), "signed in");
        app.state.overlay = None;
        app.until("online", |s| s.conn == Some(corvo_chat_core::ConnState::Online)).await;
        app
    }

    /// From `app`'s Devices window, signs out the other device.
    async fn sign_out_the_other(app: &mut App) {
        open_item(app, MenuItem::Devices).await;
        assert_eq!(form(&app.state).devices.len(), 2, "both devices are listed");
        while form(&app.state).devices[form(&app.state).selected].this_device {
            app.press(KeyCode::Down).await;
        }
        app.press(KeyCode::Char('S')).await;
        assert!(
            note(&app.state).starts_with("Sign out ") && note(&app.state).ends_with("? y/n"),
            "{}",
            note(&app.state)
        );
        assert!(app.screen(80, 24).contains("Sign out "));
        app.press(KeyCode::Char('y')).await;
        assert!(note(&app.state).starts_with("Signed out "), "{}", note(&app.state));
        assert_eq!(form(&app.state).devices.len(), 1);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn signed_out_elsewhere_goes_back_to_the_start() {
        let server = TestServer::start().await;
        let data = tempfile::tempdir().unwrap();
        let (one, two) = (data.path().join("one"), data.path().join("two"));
        let mut first = App::active(&one, &server, "alice").await;
        let username = first.state.username.clone();
        let mut second = second_device(&two, &server, &username).await;
        sign_out_the_other(&mut second).await;

        first.until("the start screen", at_start).await;
        assert_eq!(notes(&first.state), ["This device was signed out."]);
        assert!(first.screen(80, 24).contains("This device was signed out."));
        assert!(!one.join(KEY_FILE).exists(), "the key file is gone");
        assert!(!one.join(CACHE_FILE).exists(), "the saved chats are gone");
        assert!(one.join(DEVICE_ID_FILE).exists() && one.join(USERNAME_FILE).exists(), "kept for signing in again");
        second.close().await;
        first.close().await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn signed_out_while_closed_goes_back_to_the_start_at_unlock() {
        let server = TestServer::start().await;
        let data = tempfile::tempdir().unwrap();
        let (one, two) = (data.path().join("one"), data.path().join("two"));
        let first = App::active(&one, &server, "alice").await;
        let username = first.state.username.clone();
        first.close().await;
        let mut second = second_device(&two, &server, &username).await;
        sign_out_the_other(&mut second).await;

        let mut first = App::open(&one, &server);
        first.send(PASSWORD).await;
        first.until("the start screen", at_start).await;
        assert_eq!(notes(&first.state), ["This device was signed out."]);
        assert!(!one.join(KEY_FILE).exists());
        second.close().await;
        first.close().await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn change_password_saves_the_new_key_file() {
        let server = TestServer::start().await;
        let data = tempfile::tempdir().unwrap();
        let mut app = App::active(data.path(), &server, "alice").await;
        open_item(&mut app, MenuItem::Password).await;
        let labels: Vec<&str> = form(&app.state).fields.iter().map(|f| f.label).collect();
        assert_eq!(labels, ["Current password", "New password", "Again"]);
        for (i, text) in [PASSWORD, NEW_PASSWORD, NEW_PASSWORD].into_iter().enumerate() {
            if i > 0 {
                app.press(KeyCode::Tab).await;
            }
            for c in text.chars() {
                app.press(KeyCode::Char(c)).await;
            }
        }
        assert!(!app.screen(80, 24).contains("brand"), "masked");
        app.press(KeyCode::Enter).await;
        assert_eq!(note(&app.state), PASSWORD_CHANGED);
        assert!(app.screen(80, 24).contains(PASSWORD_CHANGED));
        app.press(KeyCode::Char('x')).await;
        assert_eq!(app.state.overlay, None, "any key closes it once done");
        app.close().await;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(data.path().join(KEY_FILE)).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600, "written privately");
        }

        let mut app = App::open(data.path(), &server);
        app.send(PASSWORD).await;
        assert_eq!(notes(&app.state), ["Wrong password"], "the old password no longer unlocks");
        app.send(NEW_PASSWORD).await;
        assert!(matches!(app.state.screen, Screen::Main), "the new one does");
        app.close().await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn delete_account_needs_the_word_delete() {
        let server = TestServer::start().await;
        let data = tempfile::tempdir().unwrap();
        let dir = data.path().join("alice");
        let mut app = App::active(&dir, &server, "alice").await;
        let username = app.state.username.clone();
        open_item(&mut app, MenuItem::DeleteAccount).await;
        let screen = app.screen(100, 30);
        let (first, rest) = DELETE_WARNING.split_at(DELETE_WARNING.find(" It can't").unwrap());
        assert!(screen.contains(first.split_at(50).0) && screen.contains(rest.trim()), "{screen}");
        app.press(KeyCode::Enter).await;
        assert_eq!(form(&app.state).kind, FormKind::DeleteAccount { step: DeleteStep::Password });
        app.send(PASSWORD).await;
        assert_eq!(form(&app.state).kind, FormKind::DeleteAccount { step: DeleteStep::Confirm });

        app.send("delet").await;
        assert_eq!(app.calls(), 0);
        assert!(!form(&app.state).waiting, "nothing was sent");
        assert!(note(&app.state).starts_with("That isn't delete"), "{}", note(&app.state));
        assert_eq!(form(&app.state).kind, FormKind::DeleteAccount { step: DeleteStep::Confirm });
        assert!(app.left.is_none());

        app.send("delete").await;
        assert_eq!(app.left, Some(vec!["Your account was deleted.".to_string()]));
        for file in [KEY_FILE, USERNAME_FILE, CACHE_FILE] {
            assert!(!dir.join(file).exists(), "{file} is gone");
        }
        assert!(dir.join(DEVICE_ID_FILE).exists(), "the device id stays");

        // It's gone on the server too.
        let mut other = App::open(&data.path().join("other"), &server);
        for answer in ["s", username.as_str(), PASSWORD] {
            other.send(answer).await;
        }
        assert_eq!(notes(&other.state), ["Username or password is wrong"]);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn devices_show_offline_from_the_cache() {
        let server = TestServer::start().await;
        let mut proxy = Proxy::start(&server).await;
        let data = tempfile::tempdir().unwrap();
        let mut app = App::active_at(data.path(), &server, proxy.server(&server), "alice").await;
        open_item(&mut app, MenuItem::Devices).await;
        assert!(!form(&app.state).offline);
        let screen = app.screen(80, 24);
        assert!(screen.contains("─ Devices ") && !screen.contains("(offline)"), "{screen}");
        assert!(screen.contains("this device") && screen.contains("terminal"), "{screen}");
        app.press(KeyCode::Esc).await;
        assert_eq!(app.state.overlay, None);
        app.close().await;

        proxy.cut().await;
        let mut app = App::unlock(data.path(), proxy.server(&server)).await;
        open_item(&mut app, MenuItem::Devices).await;
        assert!(form(&app.state).offline);
        assert_eq!(form(&app.state).devices.len(), 1);
        let screen = app.screen(80, 24);
        assert!(screen.contains("Devices (offline)") && screen.contains("this device"), "{screen}");
        app.close().await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn this_device_can_be_renamed_from_devices() {
        let server = TestServer::start().await;
        let data = tempfile::tempdir().unwrap();
        let mut app = App::active(data.path(), &server, "alice").await;
        open_item(&mut app, MenuItem::Devices).await;
        app.press(KeyCode::Char('r')).await;
        assert_eq!(form(&app.state).fields.len(), 1, "asks for the new name");
        while !form(&app.state).fields[0].value.is_empty() {
            app.press(KeyCode::Backspace).await;
        }
        app.send("laptop").await;
        assert_eq!(note(&app.state), "This device is now called laptop.");
        let names: Vec<&str> = form(&app.state).devices.iter().map(|d| d.name.as_str()).collect();
        assert_eq!(names, ["laptop"], "the list was asked for again");
        app.close().await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_new_display_name_shows_on_the_status_line() {
        let server = TestServer::start().await;
        let data = tempfile::tempdir().unwrap();
        let mut app = App::active(data.path(), &server, "alice").await;
        open_item(&mut app, MenuItem::Name).await;
        app.send("").await;
        assert_eq!(note(&app.state), "A display name is 1 to 32 characters.");
        app.send("Alice B").await;
        assert_eq!(app.state.overlay, None);
        assert_eq!(app.state.name, "Alice B");
        let screen = app.screen(80, 24);
        assert!(screen.lines().last().unwrap().contains("Alice B · "), "{screen}");
        assert!(screen.contains("Display name   Alice B"), "the Profile tab shows it: {screen}");
        app.close().await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_new_recovery_code_is_shown_then_checked() {
        let server = TestServer::start().await;
        let data = tempfile::tempdir().unwrap();
        let dir = data.path().join("alice");
        let mut app = App::active(&dir, &server, "alice").await;
        let username = app.state.username.clone();
        open_item(&mut app, MenuItem::RecoveryCode).await;
        app.send("not my password").await;
        assert_eq!(note(&app.state), "Wrong password");
        app.send(PASSWORD).await;
        let code = code_on(&app.screen(80, 24));
        app.press(KeyCode::Esc).await;
        assert!(app.state.overlay.is_some(), "it can't be closed before it's written down");
        app.press(KeyCode::Enter).await;
        assert!(!app.screen(80, 24).contains(&code), "hidden while its last group is typed");
        app.send("ZZZZ").await;
        assert_eq!(note(&app.state), "That isn't the last group. Look at the code again.");
        assert_eq!(code_on(&app.screen(80, 24)), code);
        app.press(KeyCode::Enter).await;
        app.send(&code[25..].to_lowercase()).await;
        assert!(form(&app.state).done);
        assert_eq!(note(&app.state), "Done: your new recovery code works from now on.");

        // The new code recovers the account elsewhere, which signs this device out.
        let mut other = App::open(&data.path().join("other"), &server);
        for answer in ["r", username.as_str(), code.as_str(), NEW_PASSWORD, NEW_PASSWORD] {
            other.send(answer).await;
        }
        assert!(other.recovery_code().is_some(), "{:?}", notes(&other.state));
        app.until("the start screen", at_start).await;
        assert_eq!(notes(&app.state), ["Your account was recovered on another device. Sign in again."]);
        app.close().await;
        other.close().await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn forgetting_the_password_at_unlock_signs_this_computer_out() {
        let server = TestServer::start().await;
        let data = tempfile::tempdir().unwrap();
        let app = App::active(data.path(), &server, "alice").await;
        let username = app.state.username.clone();
        app.close().await;

        let mut app = App::open(data.path(), &server);
        app.press(KeyCode::Esc).await;
        assert_eq!(label(&app.state), "y/n: ");
        let screen = app.screen(120, 30);
        assert!(screen.contains("Forgot your password? This signs this computer out"), "{screen}");
        app.send("x").await;
        assert_eq!(notes(&app.state), ["Type y or n."]);
        app.press(KeyCode::Esc).await;
        assert_eq!(label(&app.state), "Password: ", "Esc goes back");
        app.press(KeyCode::Esc).await;
        app.send("n").await;
        assert_eq!(label(&app.state), "Password: ");
        assert!(data.path().join(KEY_FILE).exists(), "n keeps it");
        app.press(KeyCode::Esc).await;
        app.send("y").await;

        assert!(at_start(&app.state));
        assert!(!data.path().join(KEY_FILE).exists() && !data.path().join(CACHE_FILE).exists());
        assert!(data.path().join(DEVICE_ID_FILE).exists() && data.path().join(USERNAME_FILE).exists());
        app.send("s").await;
        assert_eq!(label(&app.state), format!("Username ({username}): "), "the username is offered");
    }

    /// Ana's main screen, 80×24.
    fn ana() -> State {
        let mut state = State::new("Abin".into(), "482 910 5573".into());
        state.username = "abin-7k3q".into();
        state.size = (80, 24);
        state
    }

    fn press(state: &mut State, code: KeyCode) -> Vec<Effect> {
        update(state, Msg::Key(KeyEvent::new(code, KeyModifiers::NONE)))
    }

    fn send(state: &mut State, text: &str) -> Vec<Effect> {
        for c in text.chars() {
            press(state, KeyCode::Char(c));
        }
        press(state, KeyCode::Enter)
    }

    #[test]
    fn profile_tab_lists_the_spec_rows() {
        let mut state = ana();
        send(&mut state, "/profile");
        let buf = render(&state, 80, 24);
        let screen = screen_of(&state, 80, 24);
        assert!(screen.lines().next().unwrap().contains(" profile "), "{screen}");
        let rows: Vec<(u16, String)> =
            (0..24).map(|y| (y, (0..80).map(|x| buf[(x, y)].symbol()).collect::<String>())).collect();
        let wanted = [
            "› Display name   Abin",
            "  Your number    482 910 5573",
            "  Your username  abin-7k3q",
            "  Change password",
            "  New recovery code",
            "  Devices",
            "  Delete account",
        ];
        let found: Vec<(u16, usize)> = wanted
            .iter()
            .map(|w| {
                let (y, row) = rows.iter().find(|(_, r)| r.contains(w)).unwrap_or_else(|| panic!("{w:?}:\n{screen}"));
                (*y, row.find(w).unwrap())
            })
            .collect();
        assert!(found.windows(2).all(|p| p[0].0 + 1 == p[1].0), "in order, a row each: {found:?}");
        let (y, x) = found[6];
        let x = screen.lines().nth(y.into()).unwrap()[..x].chars().count() as u16 + 2;
        assert_eq!(buf[(x, y)].fg, Color::Red, "Delete account is red");
    }

    #[test]
    fn the_menu_skips_what_only_shows_and_esc_closes_the_tab() {
        let mut state = ana();
        let rahul = corvo_chat_proto::Number::from_body(123_456_789).unwrap();
        state.open_chat(rahul, true);
        send(&mut state, "/profile");
        let kinds: Vec<TabKind> = state.tabs.iter().map(|t| t.kind).collect();
        assert_eq!(kinds, [TabKind::Corvo, TabKind::Profile, TabKind::Chat(rahul)], "right after corvo");
        assert!(state.on_profile());
        press(&mut state, KeyCode::Down);
        assert_eq!(MENU[state.profile_at].1, MenuItem::Password, "number and username are skipped");
        press(&mut state, KeyCode::Up);
        press(&mut state, KeyCode::Up);
        assert_eq!(state.profile_at, 0);
        for _ in 0..10 {
            press(&mut state, KeyCode::Down);
        }
        assert_eq!(MENU[state.profile_at].1, MenuItem::DeleteAccount);
        assert!(press(&mut state, KeyCode::Enter).is_empty(), "nothing goes to the core yet");
        assert_eq!(form(&state).kind, FormKind::DeleteAccount { step: DeleteStep::Warn });
        press(&mut state, KeyCode::Esc);
        assert_eq!(state.overlay, None, "Esc closes the window");
        press(&mut state, KeyCode::Esc);
        assert!(!state.tabs.iter().any(|t| t.kind == TabKind::Profile), "then the tab");
        assert!(state.on_corvo());

        send(&mut state, "/profile");
        update(&mut state, Msg::Key(KeyEvent::new(KeyCode::BackTab, KeyModifiers::SHIFT)));
        assert_eq!(state.open_number(), Some(rahul), "the next tab");
        assert_eq!(state.tabs.len(), 2, "leaving the Profile tab closes it");
    }

    #[test]
    fn passwords_in_the_profile_are_masked_and_kept_out_of_the_input() {
        let mut state = ana();
        send(&mut state, "/profile");
        press(&mut state, KeyCode::Down);
        press(&mut state, KeyCode::Enter);
        for _ in 0..3 {
            for c in "hunter2 secret".chars() {
                press(&mut state, KeyCode::Char(c));
            }
            update(&mut state, Msg::Paste("pasted hunter2\n".into()));
            let screen = screen_of(&state, 80, 24);
            assert!(!screen.contains("hunter2") && screen.contains("••••"), "{screen}");
            press(&mut state, KeyCode::Tab);
        }
        let effects = press(&mut state, KeyCode::Enter);
        assert!(
            matches!(&effects[..], [Effect::Call(Command::ChangePassword { old, new })] if old == new),
            "{effects:?}"
        );
        assert!(form(&state).fields.iter().all(|f| f.value.is_empty()), "the fields were emptied");
        assert!(state.editor.is_empty());
        assert_eq!(state.history.prev("").as_deref(), Some("/profile"), "nothing typed is in the history");
        assert!(state.history.prev("").is_none());
    }

    #[test]
    fn new_passwords_are_checked_before_anything_is_sent() {
        let mut state = ana();
        state.overlay = Some(Overlay::form(Form::password()));
        let fill = |state: &mut State, texts: [&str; 3]| {
            state.overlay = Some(Overlay::form(Form::password()));
            for (i, text) in texts.into_iter().enumerate() {
                if i > 0 {
                    press(state, KeyCode::Tab);
                }
                for c in text.chars() {
                    press(state, KeyCode::Char(c));
                }
            }
            press(state, KeyCode::Enter)
        };
        assert!(fill(&mut state, [PASSWORD, "", ""]).is_empty());
        assert_eq!(note(&state), "Type all three");
        assert!(fill(&mut state, [PASSWORD, NEW_PASSWORD, "something else"]).is_empty());
        assert_eq!(note(&state), "The passwords don't match");
        assert!(fill(&mut state, [PASSWORD, "short", "short"]).is_empty());
        assert_eq!(note(&state), "At least 10 characters");
        assert!(fill(&mut state, [PASSWORD, "password123", "password123"]).is_empty());
        assert_eq!(note(&state), "That password is too common");
    }

    #[test]
    fn every_reason_to_be_signed_out_has_its_text() {
        use SignedOutReason::*;
        let said: Vec<&str> =
            [ThisDevice, ByAnotherDevice, PasswordChanged, Recovered, Expired, Banned, AccountDeleted]
                .into_iter()
                .map(signed_out_note)
                .collect();
        assert_eq!(
            said,
            [
                "This device was signed out.",
                "This device was signed out.",
                "Your password was changed on another device. Sign in again.",
                "Your account was recovered on another device. Sign in again.",
                "Your signup expired. Sign up again.",
                "This account has been banned.",
                "This account was deleted.",
            ]
        );
        let mut state = ana();
        let banned = update(&mut state, Msg::Core(corvo_chat_core::Update::SignedOut(Banned)));
        assert_eq!(banned[0], Effect::SignedOut("This account has been banned.".into()));
        // Deleted elsewhere is being signed out; asked for here, it's leaving.
        assert_eq!(signed_out(&state, AccountDeleted), Effect::SignedOut("This account was deleted.".into()));
        let mut deleting = Form::delete_account();
        deleting.kind = FormKind::DeleteAccount { step: DeleteStep::Confirm };
        deleting.waiting = true;
        state.overlay = Some(Overlay::form(deleting));
        assert_eq!(signed_out(&state, AccountDeleted), Effect::Leave(vec!["Your account was deleted.".into()]));

        // Signed out on the code screen, before the account is active.
        let mut pending = State::start(Start::new(false, None, None));
        let effects = update(&mut pending, Msg::Core(corvo_chat_core::Update::SignedOut(Expired)));
        assert_eq!(effects[0], Effect::SignedOut("Your signup expired. Sign up again.".into()));
    }
}
