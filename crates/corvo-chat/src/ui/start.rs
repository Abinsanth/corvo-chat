//! The start screens: Sign in / Sign up, recovery with the recovery code, the
//! invite code of a pending account, and unlocking. Each answer typed in the
//! input box moves it on a step. The network and file work is done by the
//! runner as [`Effect`]s, and what came of it comes back to [`Start::done`].

use chrono::{Local, TimeZone};
use corvo_chat_core::{Error, RecoveryCode, check_password};
use corvo_chat_proto::frames::{Account, ErrorCode, SignedOutReason};
use corvo_chat_proto::names::{check_base, check_display_name};
use zeroize::Zeroizing;

use super::state::{APP, Ask, Line, Secret, Tone};
use super::update::{Effect, Outcome};

pub struct Start {
    step: Step,
    /// The last username that signed in here, offered at Sign in and recovery.
    username: Option<String>,
    /// An account was made or recovered here, not just signed in or unlocked.
    made: bool,
    /// What happened since the last answer: mistakes, and why it's here.
    pub notes: Vec<Line>,
}

/// Where the start screens are, and what they have gathered so far.
enum Step {
    /// No key file: Sign in, Sign up, or recovery.
    Choose,
    /// Sign up: the username's base.
    Base,
    Name {
        base: String,
    },
    Password {
        base: String,
        name: String,
    },
    Again {
        base: String,
        name: String,
        first: Secret,
    },
    /// Sign in.
    Username,
    SignInPassword {
        username: String,
    },
    /// Recovery.
    RecoverUsername,
    RecoverCode {
        username: String,
    },
    NewPassword {
        username: String,
        code: Secret,
    },
    NewAgain {
        username: String,
        code: Secret,
        first: Secret,
    },
    /// The username and the recovery code, to write down.
    ShowCode {
        username: String,
        code: Secret,
        account: Account,
        recovered: bool,
    },
    /// The code's last group, typed back.
    LastGroup {
        username: String,
        code: Secret,
        account: Account,
        recovered: bool,
    },
    /// A pending account: the code its inviter sends.
    Invite {
        account: Account,
    },
    /// A key file: its password.
    Unlock,
    /// The runner is at work; a failure goes back to `back`.
    Waiting {
        back: Box<Step>,
    },
    /// Done: the main screen takes over.
    Entered {
        account: Account,
    },
}

impl Start {
    /// `has_key`: there's a key file here. `username`: the last one that
    /// signed in here. `note`: why it's here, like being signed out.
    pub fn new(has_key: bool, username: Option<String>, note: Option<String>) -> Start {
        let step = if has_key { Step::Unlock } else { Step::Choose };
        let notes = note.into_iter().map(|text| Line { text, tone: Tone::Warn }).collect();
        Start { step, username: username.filter(|u| !u.is_empty()), made: false, notes }
    }

    /// The account, once the start screens are done.
    pub fn entered(&self) -> Option<&Account> {
        match &self.step {
            Step::Entered { account } => Some(account),
            _ => None,
        }
    }

    /// The pending account, on the code screen.
    #[cfg(test)]
    pub fn pending(&self) -> Option<&Account> {
        match &self.step {
            Step::Invite { account } => Some(account),
            _ => None,
        }
    }

    /// The recovery code, while it's shown.
    #[cfg(test)]
    pub fn recovery_code(&self) -> Option<&str> {
        match &self.step {
            Step::ShowCode { code, .. } => Some(code.expose()),
            _ => None,
        }
    }

    /// Whether an account was made or recovered, rather than signed in or unlocked.
    pub fn made_account(&self) -> bool {
        self.made
    }

    pub fn ask(&self) -> Ask {
        let (label, masked) = match &self.step {
            Step::Choose => ("s, u or r: ", false),
            Step::Base => ("Username: ", false),
            Step::Username | Step::RecoverUsername => {
                let label = match &self.username {
                    Some(username) => format!("Username ({username}): "),
                    None => "Username: ".into(),
                };
                return Ask { label, masked: false };
            }
            Step::Name { .. } => ("Display name: ", false),
            Step::Password { .. } | Step::SignInPassword { .. } | Step::Unlock => ("Password: ", true),
            Step::Again { .. } => ("Password again: ", true),
            Step::RecoverCode { .. } => ("Recovery code: ", true),
            Step::NewPassword { .. } => ("New password: ", true),
            Step::NewAgain { .. } => ("New password again: ", true),
            Step::ShowCode { .. } => ("press Enter ", false),
            Step::LastGroup { .. } => ("Last group: ", true),
            Step::Invite { .. } => ("Invite code: ", true),
            Step::Waiting { .. } | Step::Entered { .. } => ("", false),
        };
        Ask { label: label.into(), masked }
    }

    /// What this step says above the input.
    pub fn guidance(&self) -> Vec<Line> {
        let plain = |text: &str| Line { text: text.into(), tone: Tone::Plain };
        let dim = |text: &str| Line { text: text.into(), tone: Tone::Dim };
        let back = dim("Esc goes back.");
        match &self.step {
            Step::Choose => vec![
                plain(&format!("Welcome to {APP}.")),
                plain(""),
                plain("s  Sign in"),
                plain("u  Sign up"),
                plain("r  Lost your password? Use your recovery code"),
            ],
            Step::Base => vec![
                plain("You need an invite from someone already on corvo-chat."),
                plain("Choose a username: 3 to 20 letters, digits, _ or . (four characters are added to it)."),
                back,
            ],
            Step::Name { .. } => vec![plain("Choose a display name. People you chat with see it."), back],
            Step::Password { .. } => {
                vec![plain("Choose a password: at least 10 characters. You type it each time corvo-chat starts."), back]
            }
            Step::Again { .. } => vec![plain("Type the password again."), back],
            Step::Username => vec![plain(&self.default_hint("Sign in with your username and password.")), back],
            Step::SignInPassword { username } => vec![plain(&format!("Signing in as {username}.")), back],
            Step::RecoverUsername => vec![
                plain(&self.default_hint(
                    "Recovery sets a new password and a new recovery code, and signs out your other devices.",
                )),
                back,
            ],
            Step::RecoverCode { .. } => vec![plain("Type your recovery code (six groups of four)."), back],
            Step::NewPassword { .. } => vec![plain("Choose a new password: at least 10 characters."), back],
            Step::NewAgain { .. } => vec![plain("Type the new password again."), back],
            Step::ShowCode { username, code, recovered, .. } => {
                let first = if *recovered {
                    "Your old recovery code no longer works. Write this one down.".to_string()
                } else {
                    format!("Your username is {username}. Write it down with your recovery code.")
                };
                vec![
                    plain(&first),
                    plain(""),
                    plain(&format!("    {}", code.expose())),
                    plain(""),
                    plain("It's the only way back in if you forget your password. Press Enter once it's written down."),
                ]
            }
            Step::LastGroup { .. } => vec![plain("Type the last group of your recovery code")],
            Step::Invite { account } => vec![plain(&invite_text(account))],
            Step::Unlock => {
                let who = self.username.as_ref().map_or(String::new(), |u| format!(" ({u})"));
                vec![plain(&format!("Type your password to unlock corvo-chat{who}."))]
            }
            Step::Waiting { .. } | Step::Entered { .. } => Vec::new(),
        }
    }

    /// `lead`, then how to keep the remembered username, if there is one.
    fn default_hint(&self, lead: &str) -> String {
        match &self.username {
            Some(username) => format!("{lead} Press Enter for {username}, or type another username."),
            None => lead.into(),
        }
    }

    fn note(&mut self, tone: Tone, text: impl Into<String>) {
        self.notes.push(Line { text: text.into(), tone });
    }

    /// A username typed, or the remembered one for an empty answer.
    fn username_from(&self, text: &str) -> Option<String> {
        let typed = text.trim().to_lowercase();
        if typed.is_empty() { self.username.clone() } else { Some(typed) }
    }

    /// Esc on an empty input: back to Sign in / Sign up from a step of one
    /// of its flows.
    pub fn back(&mut self) {
        let in_a_flow = matches!(
            self.step,
            Step::Base
                | Step::Name { .. }
                | Step::Password { .. }
                | Step::Again { .. }
                | Step::Username
                | Step::SignInPassword { .. }
                | Step::RecoverUsername
                | Step::RecoverCode { .. }
                | Step::NewPassword { .. }
                | Step::NewAgain { .. }
        );
        if in_a_flow {
            self.notes.clear();
            self.step = Step::Choose;
        }
    }

    /// An answer typed and sent with Enter.
    pub fn answer(&mut self, text: Zeroizing<String>) -> Vec<Effect> {
        self.notes.clear();
        let text = Secret::new(text);
        let mut effects = Vec::new();
        let step = std::mem::replace(&mut self.step, Step::Choose);
        self.step = match step {
            Step::Choose => match text.expose().trim().to_lowercase().as_str() {
                "s" => Step::Username,
                "u" => Step::Base,
                "r" => Step::RecoverUsername,
                _ => {
                    self.note(Tone::Bad, "Type s to sign in, u to sign up, or r to use your recovery code.");
                    Step::Choose
                }
            },
            Step::Base => match check_base(text.expose()) {
                Ok(base) => Step::Name { base },
                Err(_) => {
                    self.note(Tone::Bad, "Use 3 to 20 letters, digits, _ or ., starting with a letter.");
                    Step::Base
                }
            },
            Step::Name { base } => match check_display_name(text.expose()) {
                Ok(name) => Step::Password { base, name },
                Err(_) => {
                    self.note(Tone::Bad, "A display name is 1 to 32 characters.");
                    Step::Name { base }
                }
            },
            Step::Password { base, name } => match check_password(text.expose()) {
                Ok(()) => Step::Again { base, name, first: text },
                Err(e) => {
                    self.note(Tone::Bad, say(&e));
                    Step::Password { base, name }
                }
            },
            Step::Again { base, name, first } if text != first => {
                self.note(Tone::Bad, "The passwords don't match");
                Step::Password { base, name }
            }
            Step::Again { base, name, first } => {
                effects.push(Effect::SignUp { base, name, password: first });
                Step::Waiting { back: Box::new(Step::Base) }
            }
            Step::Username => match self.username_from(text.expose()) {
                Some(username) => Step::SignInPassword { username },
                None => {
                    self.note(Tone::Bad, "Type your username");
                    Step::Username
                }
            },
            // Only the length: a password once fine stays fine to sign in with.
            Step::SignInPassword { username }
                if matches!(check_password(text.expose()), Err(Error::PasswordTooShort)) =>
            {
                self.note(Tone::Bad, say(&Error::PasswordTooShort));
                Step::SignInPassword { username }
            }
            Step::SignInPassword { username } => {
                effects.push(Effect::SignIn { username: username.clone(), password: text });
                Step::Waiting { back: Box::new(Step::SignInPassword { username }) }
            }
            Step::RecoverUsername => match self.username_from(text.expose()) {
                Some(username) => Step::RecoverCode { username },
                None => {
                    self.note(Tone::Bad, "Type your username");
                    Step::RecoverUsername
                }
            },
            Step::RecoverCode { username } => match RecoveryCode::parse(text.expose()) {
                Ok(_) => Step::NewPassword { username, code: text },
                Err(e) => {
                    self.note(Tone::Bad, say(&e));
                    Step::RecoverCode { username }
                }
            },
            Step::NewPassword { username, code } => match check_password(text.expose()) {
                Ok(()) => Step::NewAgain { username, code, first: text },
                Err(e) => {
                    self.note(Tone::Bad, say(&e));
                    Step::NewPassword { username, code }
                }
            },
            Step::NewAgain { username, code, first } if text != first => {
                self.note(Tone::Bad, "The passwords don't match");
                Step::NewPassword { username, code }
            }
            Step::NewAgain { username, code, first } => {
                effects.push(Effect::Recover { username: username.clone(), code, password: first });
                Step::Waiting { back: Box::new(Step::RecoverCode { username }) }
            }
            Step::ShowCode { username, code, account, recovered } => {
                Step::LastGroup { username, code, account, recovered }
            }
            Step::LastGroup { username, code, account, recovered } => {
                if is_last_group(text.expose(), code.expose()) {
                    self.after_code(account)
                } else {
                    self.note(Tone::Bad, "That isn't the last group. Look at the code again.");
                    Step::ShowCode { username, code, account, recovered }
                }
            }
            Step::Invite { account } => {
                let code: String = text.expose().chars().filter(|c| !c.is_whitespace() && *c != '-').collect();
                if code.is_empty() {
                    self.note(Tone::Bad, "Type the code they sent you");
                    Step::Invite { account }
                } else {
                    effects.push(Effect::Activate { code: Secret::new(code) });
                    Step::Waiting { back: Box::new(Step::Invite { account }) }
                }
            }
            Step::Unlock if text.expose().is_empty() => {
                self.note(Tone::Bad, "Type your password");
                Step::Unlock
            }
            Step::Unlock => {
                effects.push(Effect::Unlock { password: text });
                Step::Waiting { back: Box::new(Step::Unlock) }
            }
            // Nothing is asked while waiting, or once done.
            step @ (Step::Waiting { .. } | Step::Entered { .. }) => step,
        };
        effects
    }

    /// The code screen for a pending account, or done for an active one.
    fn after_code(&mut self, account: Account) -> Step {
        if account.pending_until.is_some() { Step::Invite { account } } else { Step::Entered { account } }
    }

    /// What came of an effect the start screens asked for.
    pub fn done(&mut self, outcome: Outcome) -> Vec<Effect> {
        self.notes.clear();
        let step = std::mem::replace(&mut self.step, Step::Choose);
        self.step = match (step, outcome) {
            (Step::Waiting { back }, Outcome::SignedUp { username, code, account }) => {
                let recovered = matches!(*back, Step::RecoverCode { .. });
                self.made = true;
                self.username = Some(username.clone());
                Step::ShowCode { username, code, account, recovered }
            }
            (Step::Waiting { .. }, Outcome::Running { account }) => Step::Entered { account },
            (Step::Waiting { .. }, Outcome::Pending { account }) => Step::Invite { account },
            (Step::Waiting { back }, Outcome::Failed(why)) => {
                self.note(Tone::Bad, why);
                *back
            }
            (step, _) => step,
        };
        Vec::new()
    }
}

/// Whether `typed` is the last group of `code` (shown as six groups joined
/// by dashes), ignoring case and spaces, with Crockford's look-alikes.
fn is_last_group(typed: &str, code: &str) -> bool {
    let typed: String = typed
        .chars()
        .filter(|c| !c.is_whitespace() && *c != '-')
        .map(|c| match c.to_ascii_uppercase() {
            'I' | 'L' => '1',
            'O' => '0',
            c => c,
        })
        .collect();
    code.rsplit('-').next().is_some_and(|last| !last.is_empty() && typed == last)
}

/// The code screen's text, with the time in local time.
fn invite_text(account: &Account) -> String {
    let until = account
        .pending_until
        .and_then(|secs| Local.timestamp_opt(secs, 0).single())
        .map_or_else(String::new, |t| format!(" You have until {}.", t.format("%-d %b %H:%M")));
    format!(
        "Your number is {}. Give it to the person inviting you, then enter the 6-digit code they send you.{until}",
        account.number
    )
}

/// "Locked for N more minutes", counted up from `until - now` seconds.
fn locked_for(until: i64, now: i64) -> String {
    match (until - now + 59).max(60) / 60 {
        1 => "Locked for 1 more minute".into(),
        n => format!("Locked for {n} more minutes"),
    }
}

fn now() -> i64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs() as i64)
}

/// What an error from the core says on the start screens: the one place
/// errors become texts there.
pub fn say(error: &Error) -> String {
    match error {
        Error::WrongCredentials => "Username or password is wrong".into(),
        Error::WrongPassword => "Wrong password".into(),
        Error::PasswordTooShort => "At least 10 characters".into(),
        Error::PasswordTooCommon => "That password is too common".into(),
        Error::Server { code: ErrorCode::Locked, until: Some(until), .. } => locked_for(*until, now()),
        Error::Server { code: ErrorCode::LockedForever, .. } => "Locked. Sign in with your recovery code".into(),
        Error::Server { code: ErrorCode::DeviceLimit, .. } => {
            "Maximum devices reached (3). Sign out a device first.".into()
        }
        Error::Server { code: ErrorCode::SignInLimit, .. } => "Too many sign-ins today, try again tomorrow.".into(),
        Error::Server { code: ErrorCode::SignupExpired, .. } | Error::SignedOut(SignedOutReason::Expired) => {
            "Your signup expired. Sign up again.".into()
        }
        Error::Server { message, .. } => message.clone(),
        other => other.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::files::{DEVICE_ID_FILE, KEY_FILE, USERNAME_FILE};
    use crate::ui::runner::testing::{App, PASSWORD, TestServer};
    use crate::ui::state::{Screen, State};
    use crate::ui::update::{Msg, update};
    use crate::ui::view::render;
    use corvo_chat_core::ConnState;
    use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

    fn screen_text(state: &State) -> String {
        let buf = render(state, 100, 30);
        (0..buf.area.height)
            .map(|y| (0..buf.area.width).map(|x| buf[(x, y)].symbol()).collect::<String>())
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn start(state: &State) -> &Start {
        match &state.screen {
            Screen::Start(start) => start,
            Screen::Main => panic!("the start screens are over"),
        }
    }

    fn label(state: &State) -> String {
        state.ask().expect("a question").label
    }

    /// The recovery code on screen, from the guidance.
    fn shown_code(state: &State) -> String {
        start(state)
            .guidance()
            .iter()
            .map(|l| l.text.trim().to_string())
            .find(|t| t.len() == 29 && t.matches('-').count() == 5)
            .expect("a recovery code is shown")
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn sign_up_then_invite_then_main() {
        let server = TestServer::start().await;
        let data = tempfile::tempdir().unwrap();
        let mut app = App::open(data.path(), &server);
        app.send("u").await;
        assert!(screen_text(&app.state).contains("You need an invite from someone already on corvo-chat."));
        app.send("abin").await;
        app.send("Abin").await;
        app.send(PASSWORD).await;
        app.send(PASSWORD).await;

        let text = screen_text(&app.state);
        assert!(text.contains("Your username is abin-"), "{text}");
        let code = shown_code(&app.state);
        app.send("").await;
        assert_eq!(label(&app.state), "Last group: ");
        app.send("ZZZZ").await;
        assert!(screen_text(&app.state).contains("That isn't the last group"));
        assert!(screen_text(&app.state).contains(&code), "the code is shown again");
        app.send("").await;
        app.send(&code[25..].to_lowercase()).await;

        let text = screen_text(&app.state);
        assert!(text.contains("Your number is"), "{text}");
        assert_eq!(label(&app.state), "Invite code: ");
        app.until("online", |s| s.conn == Some(ConnState::Online)).await;
        let invite = server.invite(app.number());
        let wrong = if invite == "000000" { "000001" } else { "000000" };
        app.send(wrong).await;
        assert!(screen_text(&app.state).contains("That code is wrong."), "the server's message");
        assert_eq!(label(&app.state), "Invite code: ", "the screen stays");
        app.send(&invite).await;
        assert!(matches!(app.state.screen, Screen::Main));
        assert_eq!(app.state.name, "Abin");

        for file in [KEY_FILE, DEVICE_ID_FILE, USERNAME_FILE] {
            assert!(data.path().join(file).exists(), "{file}");
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(data.path().join(KEY_FILE)).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600);
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn wrong_username_and_password_look_the_same() {
        let server = TestServer::start().await;
        let (username, _) = App::signed_up_elsewhere(&server, "abin").await;
        let data = tempfile::tempdir().unwrap();
        let mut said = Vec::new();
        for (who, password) in [(username.as_str(), "not the password"), ("nobody-zzzz", PASSWORD)] {
            let mut app = App::open(data.path(), &server);
            app.send("s").await;
            app.send(who).await;
            app.send(password).await;
            let notes: Vec<String> = start(&app.state).notes.iter().map(|n| n.text.clone()).collect();
            assert_eq!(label(&app.state), "Password: ", "{who}: stays on the password prompt");
            said.push(notes);
        }
        assert_eq!(said[0], ["Username or password is wrong"]);
        assert_eq!(said[0], said[1]);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn wrong_password_at_unlock_asks_again() {
        let server = TestServer::start().await;
        let data = tempfile::tempdir().unwrap();
        App::active(data.path(), &server, "abin").await.close().await;

        let mut app = App::open(data.path(), &server);
        assert_eq!(label(&app.state), "Password: ");
        assert!(app.state.ask().unwrap().masked);
        app.send("not the password").await;
        assert!(screen_text(&app.state).contains("Wrong password"));
        assert_eq!(label(&app.state), "Password: ");
        app.send(PASSWORD).await;
        assert!(matches!(app.state.screen, Screen::Main));
        assert_eq!(app.state.name, "abin");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn recovery_shows_a_new_code() {
        let server = TestServer::start().await;
        let (username, code) = App::signed_up_elsewhere(&server, "abin").await;
        let data = tempfile::tempdir().unwrap();
        let mut app = App::open(data.path(), &server);
        app.send("r").await;
        app.send(&username).await;
        app.send(code.expose()).await;
        let new_password = "a brand new password";
        app.send(new_password).await;
        app.send(new_password).await;
        let text = screen_text(&app.state);
        assert!(text.contains("Your old recovery code no longer works."), "{text}");
        let new_code = shown_code(&app.state);
        assert_ne!(new_code, code.expose());
        app.send("").await;
        app.send(&new_code[25..]).await;
        assert_eq!(label(&app.state), "Invite code: ", "still pending");
        let invite = server.invite(app.number());
        app.until("online", |s| s.conn == Some(ConnState::Online)).await;
        app.send(&invite).await;
        assert!(matches!(app.state.screen, Screen::Main));
        app.close().await;

        // The new password signs in on another device.
        let other = tempfile::tempdir().unwrap();
        let mut app = App::open(other.path(), &server);
        app.send("s").await;
        app.send(&username).await;
        app.send(new_password).await;
        assert!(matches!(app.state.screen, Screen::Main));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_weak_password_never_reaches_the_server() {
        let server = TestServer::start().await;
        let data = tempfile::tempdir().unwrap();
        let mut app = App::open(data.path(), &server);
        app.send("u").await;
        app.send("abin").await;
        app.send("Abin").await;
        app.send("short").await;
        assert!(screen_text(&app.state).contains("At least 10 characters"));
        assert_eq!(label(&app.state), "Password: ");
        app.send("password123").await;
        assert!(screen_text(&app.state).contains("That password is too common"));
        app.send(PASSWORD).await;
        app.send("something else entirely").await;
        assert!(screen_text(&app.state).contains("The passwords don't match"));
        assert_eq!(label(&app.state), "Password: ");
        assert_eq!(app.connections(), 0, "the runner made no connection");
    }

    #[test]
    fn passwords_are_masked_and_kept_out_of_history() {
        let mut s = State::start(Start::new(true, Some("abin-7k3q".into()), None));
        let press = |s: &mut State, code| update(s, Msg::Key(KeyEvent::new(code, KeyModifiers::NONE)));
        for c in "hunter2 secret".chars() {
            press(&mut s, KeyCode::Char(c));
        }
        assert!(s.editor.capacity() >= crate::ui::state::ANSWER_ROOM, "room made before typing");
        let text = screen_text(&s);
        assert!(text.contains("Password: ••••••••••••••"), "{text}");
        assert!(!text.contains("hunter2"));
        let effects = press(&mut s, KeyCode::Enter);
        assert!(effects.contains(&Effect::Unlock { password: Secret::new(Zeroizing::new("hunter2 secret".into())) }));
        assert_eq!(s.busy, Some("Unlocking…"));
        update(&mut s, Msg::Done(Outcome::Failed("Wrong password".into())));
        press(&mut s, KeyCode::Up);
        assert!(s.editor.is_empty(), "↑ recalls nothing");
        assert!(s.history.prev("").is_none(), "nothing typed is in the history");
    }

    #[test]
    fn choose_takes_s_u_or_r_and_esc_goes_back() {
        let mut s = State::start(Start::new(false, None, Some("This device was signed out.".into())));
        assert!(screen_text(&s).contains("This device was signed out."));
        assert!(screen_text(&s).contains("r  Lost your password? Use your recovery code"));
        let send = |s: &mut State, text: &str| {
            for c in text.chars() {
                update(s, Msg::Key(KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE)));
            }
            update(s, Msg::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)))
        };
        send(&mut s, "x");
        assert_eq!(label(&s), "s, u or r: ");
        send(&mut s, " S ");
        assert_eq!(label(&s), "Username: ");
        update(&mut s, Msg::Key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE)));
        assert_eq!(label(&s), "s, u or r: ");
        send(&mut s, "u");
        send(&mut s, "1abc");
        assert_eq!(label(&s), "Username: ", "a base must start with a letter");
    }

    #[test]
    fn the_remembered_username_is_offered() {
        let mut s = Start::new(false, Some("abin-7k3q".into()), None);
        s.answer(Zeroizing::new("s".into()));
        assert_eq!(s.ask().label, "Username (abin-7k3q): ");
        s.answer(Zeroizing::new(String::new()));
        assert!(s.guidance()[0].text.contains("Signing in as abin-7k3q."));
    }

    #[test]
    fn errors_become_the_spec_texts() {
        let server = |code, until| Error::Server { code, message: "from the server".into(), until };
        assert_eq!(say(&Error::WrongCredentials), "Username or password is wrong");
        assert_eq!(say(&server(ErrorCode::LockedForever, None)), "Locked. Sign in with your recovery code");
        assert_eq!(say(&server(ErrorCode::DeviceLimit, None)), "Maximum devices reached (3). Sign out a device first.");
        assert_eq!(say(&server(ErrorCode::SignInLimit, None)), "Too many sign-ins today, try again tomorrow.");
        assert_eq!(say(&server(ErrorCode::SignupExpired, None)), "Your signup expired. Sign up again.");
        assert_eq!(say(&server(ErrorCode::CodeWrong, None)), "from the server");
        assert_eq!(say(&server(ErrorCode::Locked, Some(now() + 600))), "Locked for 10 more minutes");
        assert_eq!(locked_for(1_000 + 14 * 60 + 1, 1_000), "Locked for 15 more minutes");
        assert_eq!(locked_for(1_030, 1_000), "Locked for 1 more minute");
    }

    #[test]
    fn the_last_group_ignores_case_and_look_alikes() {
        let code = "ABCD-EFGH-JKMN-PQRS-TVWX-Y01Z";
        assert!(is_last_group("y01z", code));
        assert!(is_last_group(" YOIZ ", code));
        assert!(!is_last_group("TVWX", code));
        assert!(!is_last_group("", code));
    }
}
