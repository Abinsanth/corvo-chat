//! The people commands: `/accept`, `/reject`, `/block`, `/unblock`,
//! `/blocked`, `/contacts`, `/requests`, `/remove`, `/info`, `/verify`,
//! `/report` and `/invite`, the questions some of them ask in the input
//! box, and the warning when someone's keys change.

use chrono::{Local, TimeZone};
use corvo_chat_core::native::{Command, Reply};
use corvo_chat_core::{Error, PersonRow, parse_number};
use corvo_chat_proto::Number;
use corvo_chat_proto::chat::Relation;
use corvo_chat_proto::frames::InviteInfo;

use super::chat;
use super::commands::Cmd;
use super::start::say;
use super::state::{Ask, State, Tone};
use super::table::{cut, table};
use super::update::Effect;
use super::view::clean;
use super::window::Overlay;

/// What a number with a wrong check digit gets.
pub const TYPO: &str = "That number has a typo";
/// The longest reason a report takes, in characters (the server's limit).
pub const REASON_LEN: usize = 200;
/// The widest a name gets in a table.
const NAME_WIDTH: usize = 16;

/// A command in the middle of asking its questions in the input box.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Pending {
    /// `/remove`: delete the history with them too?
    Remove { number: Number },
    /// `/report`: why, if they like.
    Reason { number: Number },
    /// `/report`: block them too?
    Block { number: Number, reason: Option<String> },
}

impl Pending {
    pub fn ask(&self) -> Ask {
        let label = match self {
            Pending::Remove { .. } => "Delete the history too? y/n: ",
            Pending::Reason { .. } => "Reason (optional, Enter to skip): ",
            Pending::Block { .. } => "Block them too? y/n: ",
        };
        Ask { label: label.into(), masked: false }
    }
}

/// Runs one of the people commands.
pub fn command(state: &mut State, cmd: Cmd, effects: &mut Vec<Effect>) {
    match cmd {
        Cmd::Contacts => contacts(state),
        Cmd::Requests => requests(state),
        Cmd::Blocked => blocked(state),
        Cmd::Invite(None) => effects.push(Effect::Call(Command::Invites)),
        Cmd::Invite(Some(typed)) => match number(&typed) {
            Err(why) => state.say(Tone::Bad, why),
            Ok(number) if Some(number) == state.me => state.say(Tone::Bad, "That's your own number."),
            Ok(number) => effects.push(Effect::Call(Command::Invite { number })),
        },
        Cmd::Accept(typed) => {
            let ask = "Which request? /accept <name|number>, or open their chat first.";
            if let Some(number) = who(state, typed.as_deref(), ask)
                && is(state, number, Relation::Request)
            {
                effects.push(Effect::Call(Command::Accept { number }));
            }
        }
        Cmd::Reject(typed) => {
            let ask = "Which request? /reject <name|number>, or open their chat first.";
            if let Some(number) = who(state, typed.as_deref(), ask)
                && is(state, number, Relation::Request)
            {
                effects.push(Effect::Call(Command::Reject { number }));
            }
        }
        Cmd::Block(typed) => {
            let Some(number) = who(state, typed.as_deref(), "Who? /block <name|number>, or open their chat first.")
            else {
                return;
            };
            // Anyone can be blocked, even someone not known here yet.
            if relation(state, number) == Some(Relation::Blocked) {
                let label = state.label(number);
                state.say(Tone::Bad, format!("{label} is already blocked."));
            } else {
                effects.push(Effect::Call(Command::Block { number }));
            }
        }
        Cmd::Unblock(typed) => {
            if let Some(number) = who(state, Some(&typed), "")
                && is(state, number, Relation::Blocked)
            {
                effects.push(Effect::Call(Command::Unblock { number }));
            }
        }
        Cmd::Remove(typed) => {
            let Some(number) = who(state, Some(&typed), "") else { return };
            let label = state.label(number);
            match relation(state, number) {
                Some(Relation::Contact) => {
                    state.select_tab(0);
                    state.say(Tone::Plain, format!("Removing {label}. Esc cancels."));
                    state.pending = Some(Pending::Remove { number });
                }
                Some(Relation::Request) => state.say(Tone::Bad, format!("{label} is a request: /reject drops it.")),
                _ => state.say(Tone::Bad, format!("{label} isn't a contact.")),
            }
        }
        Cmd::Report(typed) => {
            let Some(number) = who(state, Some(&typed), "") else { return };
            let label = state.label(number);
            state.select_tab(0);
            state.say(Tone::Plain, format!("Reporting {label} to the server. Esc cancels."));
            state.pending = Some(Pending::Reason { number });
        }
        Cmd::Verify(typed) => {
            if let Some(number) = who(state, Some(&typed), "") {
                effects.push(Effect::Call(Command::Verify { number }));
            }
        }
        Cmd::Info(typed) => {
            let Some(number) = who(state, Some(&typed), "") else { return };
            match state.person(number).cloned() {
                Some(person) => state.overlay = Some(Overlay::Info { rows: info(&person), scroll: 0 }),
                None => state.say(Tone::Bad, format!("{number} isn't someone you know here.")),
            }
        }
        _ => {}
    }
}

/// Who a command means: the one `typed`, or else the person in the open
/// chat tab. Says why not (`ask` when there's neither), including for your
/// own number.
fn who(state: &mut State, typed: Option<&str>, ask: &str) -> Option<Number> {
    let found = match typed {
        Some(typed) => resolve(state, typed),
        None => state.open_number().ok_or_else(|| ask.to_string()),
    };
    match found {
        Ok(number) if Some(number) == state.me => {
            state.say(Tone::Bad, "That's your own number.");
            None
        }
        Ok(number) => Some(number),
        Err(why) => {
            state.say(Tone::Bad, why);
            None
        }
    }
}

fn relation(state: &State, number: Number) -> Option<Relation> {
    state.person(number).map(|p| p.relation)
}

/// Whether `number` stands as `wanted`; if not, says why.
fn is(state: &mut State, number: Number, wanted: Relation) -> bool {
    let now = relation(state, number);
    if now == Some(wanted) {
        return true;
    }
    let label = state.label(number);
    let why = match (now, wanted) {
        (Some(Relation::Blocked), _) => format!("You've blocked {label}: /unblock them first."),
        (Some(Relation::Contact), Relation::Request) => format!("{label} is already a contact."),
        (_, Relation::Blocked) => format!("{label} isn't blocked."),
        _ => format!("{label} isn't a request."),
    };
    state.say(Tone::Bad, why);
    false
}

/// An answer to the pending question.
pub fn answer(state: &mut State, text: &str) -> Vec<Effect> {
    let Some(pending) = state.pending.take() else { return Vec::new() };
    let mut effects = Vec::new();
    let yes = || match text.trim().to_lowercase().as_str() {
        "y" | "yes" => Some(true),
        "n" | "no" => Some(false),
        _ => None,
    };
    state.pending = match pending {
        Pending::Remove { number } => match yes() {
            Some(clear_history) => {
                effects.push(Effect::Call(Command::Remove { number, clear_history }));
                None
            }
            None => again(state, Pending::Remove { number }),
        },
        Pending::Reason { number } => {
            let reason: String = text.trim().chars().take(REASON_LEN).collect();
            let reason = Some(reason.trim().to_string()).filter(|r| !r.is_empty());
            Some(Pending::Block { number, reason })
        }
        Pending::Block { number, reason } => match yes() {
            Some(block) => {
                effects.push(Effect::Call(Command::Report { number, reason, block }));
                None
            }
            None => again(state, Pending::Block { number, reason }),
        },
    };
    effects
}

/// The same question again, after an answer that wasn't y or n.
fn again(state: &mut State, pending: Pending) -> Option<Pending> {
    state.say(Tone::Bad, "Type y or n.");
    Some(pending)
}

/// Esc: stops asking.
pub fn cancel(state: &mut State) {
    if state.pending.take().is_some() {
        state.say(Tone::Plain, "Cancelled.");
    }
}

/// The core's answer to one of the people commands.
pub fn reply(state: &mut State, asked: Command, reply: Result<Reply, String>, effects: &mut Vec<Effect>) {
    let reply = match reply {
        Ok(reply) => reply,
        Err(why) => {
            let why = match asked {
                Command::Verify { number } if why == say(&Error::NotPinned) => {
                    let label = state.label(number);
                    format!("There are no keys for {label} here yet: send them a message, or /add {number}, first.")
                }
                _ => why,
            };
            return state.say(Tone::Bad, why);
        }
    };
    let label = |state: &State, number| state.label(number);
    let changes_people = matches!(
        asked,
        Command::Accept { .. }
            | Command::Reject { .. }
            | Command::Block { .. }
            | Command::Unblock { .. }
            | Command::Remove { .. }
            | Command::Report { block: true, .. }
    );
    match (asked, reply) {
        (Command::Accept { number }, _) => {
            let label = label(state, number);
            state.say(Tone::Good, format!("{label} is now a contact."));
        }
        (Command::Reject { number }, _) => {
            let label = label(state, number);
            state.close_chat(number);
            state.say(Tone::Good, format!("Dropped {label}'s request. They aren't told."));
        }
        (Command::Block { number }, _) => {
            let label = label(state, number);
            state.close_chat(number);
            state.say(Tone::Good, format!("Blocked {label}: nothing from them reaches you. /unblock undoes it."));
        }
        (Command::Unblock { number }, _) => {
            let label = label(state, number);
            state.say(Tone::Good, format!("Unblocked {label}. If they write, it's a new request."));
        }
        (Command::Remove { number, clear_history }, _) => {
            let label = label(state, number);
            state.close_chat(number);
            let history = if clear_history { " and your history with them" } else { "; your history is kept" };
            state.say(Tone::Good, format!("Removed {label}{history}."));
        }
        (Command::Report { number, block, .. }, _) => {
            let label = label(state, number);
            if block {
                state.close_chat(number);
                state.say(Tone::Good, format!("Reported and blocked {label}."));
            } else {
                state.say(Tone::Good, format!("Reported {label}."));
            }
        }
        (Command::Verify { number }, Reply::SafetyCode(code)) => {
            let name = clean(&label(state, number));
            state.overlay = Some(Overlay::SafetyCode { name, code, scroll: 0 });
        }
        (Command::Invite { .. }, Reply::Invite(invite)) => {
            let name = shown_name(&invite);
            let (code, until) = (code(&invite.code), until(invite.expires_at));
            state.select_tab(0);
            state.say(Tone::Plain, format!("Invite code for {name}: {code}. Send it to them. It works until {until}."));
        }
        (Command::Invites, Reply::Invites(invites)) => invites_table(state, &invites),
        _ => {}
    }
    if changes_people {
        chat::reload_all(state, effects);
    }
}

/// An invitee's name as shown: cleaned, or their number.
fn shown_name(invite: &InviteInfo) -> String {
    let name = clean(&invite.display_name.replace('\n', " "));
    if name.trim().is_empty() { invite.number.to_string() } else { name.trim().to_string() }
}

/// A 6-digit invite code as `123 456`.
fn code(code: &str) -> String {
    match (code.len(), code.get(..3), code.get(3..)) {
        (6, Some(a), Some(b)) => format!("{a} {b}"),
        _ => clean(code),
    }
}

/// When an invite stops working, in local time.
fn until(secs: i64) -> String {
    Local.timestamp_opt(secs, 0).single().map_or_else(|| "?".into(), |t| t.format("%-d %b %H:%M").to_string())
}

fn invites_table(state: &mut State, invites: &[InviteInfo]) {
    state.select_tab(0);
    if invites.is_empty() {
        return state.say(Tone::Plain, "No open invites. /invite <number> makes one.");
    }
    let rows: Vec<Vec<String>> = invites
        .iter()
        .map(|i| vec![cut(&shown_name(i), NAME_WIDTH), i.number.to_string(), code(&i.code), until(i.expires_at)])
        .collect();
    say_table(state, &["NAME", "NUMBER", "CODE", "UNTIL"], &rows);
}

/// A person's label for a table: cleaned, on one line, cut to fit.
fn name_cell(state: &State, number: Number) -> String {
    cut(clean(&state.label(number).replace('\n', " ")).trim(), NAME_WIDTH)
}

/// The people standing as `relation`, A to Z.
fn of(state: &State, relation: Relation) -> Vec<PersonRow> {
    let mut people: Vec<PersonRow> = state.people.iter().filter(|p| p.relation == relation).cloned().collect();
    people.sort_by_key(|p| (name_cell(state, p.number).to_lowercase(), p.number.digits()));
    people
}

fn unread(state: &State, number: Number) -> usize {
    state.list.iter().filter(|r| r.with == number).map(|r| r.unread).sum()
}

fn request_rows(state: &State) -> Vec<Vec<String>> {
    of(state, Relation::Request)
        .iter()
        .map(|p| vec![name_cell(state, p.number), unread(state, p.number).to_string(), p.number.to_string()])
        .collect()
}

/// `/contacts`: contacts A to Z, then requests.
fn contacts(state: &mut State) {
    state.select_tab(0);
    let rows: Vec<Vec<String>> = of(state, Relation::Contact)
        .iter()
        .map(|p| {
            let status = match (p.mismatch, p.online) {
                (true, _) => "⚠ keys changed",
                (false, true) => "● online",
                (false, false) => "○ offline",
            };
            vec![name_cell(state, p.number), status.to_string(), p.number.to_string()]
        })
        .collect();
    let requests = request_rows(state);
    if rows.is_empty() && requests.is_empty() {
        return state.say(Tone::Plain, "No contacts yet: /add <number> adds a friend.");
    }
    if rows.is_empty() {
        state.say(Tone::Plain, "No contacts yet.");
    } else {
        say_table(state, &["NAME", "STATUS", "NUMBER"], &rows);
    }
    if requests.is_empty() {
        state.say(Tone::Dim, "/chat <name> opens a chat · /info <name> for more");
    } else {
        state.say(Tone::Plain, "");
        say_table(state, &["REQUESTS", "UNREAD", "NUMBER"], &requests);
        state.say(Tone::Dim, "/chat <name> opens a chat · /accept <name> adds them");
    }
}

/// `/requests`: the people waiting for you, A to Z.
fn requests(state: &mut State) {
    state.select_tab(0);
    let rows = request_rows(state);
    if rows.is_empty() {
        return state.say(Tone::Plain, "No requests.");
    }
    say_table(state, &["NAME", "UNREAD", "NUMBER"], &rows);
    state.say(Tone::Dim, "/chat <name> to read · /accept <name> adds them · /reject <name> drops them");
}

/// `/blocked`: the people you blocked, A to Z.
fn blocked(state: &mut State) {
    state.select_tab(0);
    let rows: Vec<Vec<String>> =
        of(state, Relation::Blocked).iter().map(|p| vec![name_cell(state, p.number), p.number.to_string()]).collect();
    if rows.is_empty() {
        return state.say(Tone::Plain, "You haven't blocked anyone.");
    }
    say_table(state, &["NAME", "NUMBER"], &rows);
    state.say(Tone::Dim, "/unblock <name> lets them back");
}

/// A table in the corvo tab, its heading dim.
fn say_table(state: &mut State, head: &[&str], rows: &[Vec<String>]) {
    let mut lines = table(head, rows).into_iter();
    if let Some(heading) = lines.next() {
        state.say(Tone::Dim, heading);
    }
    for line in lines {
        state.say(Tone::Plain, line);
    }
}

/// `/info`'s rows: name, number, relation, and online for a contact.
fn info(person: &PersonRow) -> Vec<String> {
    let field = |name: &str, value: &str| format!("{name:<11}{value}");
    let name = person.name.as_deref().map(|n| clean(&n.replace('\n', " ")).trim().to_string());
    let name = name.filter(|n| !n.is_empty()).unwrap_or_else(|| "(not shared)".into());
    let relation = match person.relation {
        Relation::Contact => "contact",
        Relation::Request => "request (not a contact yet)",
        Relation::Asked => "asked (they haven't accepted yet)",
        Relation::Blocked => "blocked",
        Relation::None => "not a contact",
    };
    let mut rows =
        vec![field("Name:", &name), field("Number:", &person.number.to_string()), field("Relation:", relation)];
    if person.relation == Relation::Contact {
        rows.push(field("Online:", if person.online { "yes" } else { "no" }));
    }
    if person.mismatch {
        rows.push(field("Keys:", "⚠ changed. Check with /verify."));
    }
    if person.deleted {
        rows.push(field("Account:", "deleted"));
    }
    rows
}

/// Decision 10's warning about `label`.
pub fn key_warning(label: &str) -> String {
    format!("⚠ {label}'s keys changed. Nothing will be sent to them. Check with /verify.")
}

/// Says, once, that someone's keys changed, unless their tab is open
/// (it says so at the top). Someone whose keys check out again can be
/// warned about again.
pub fn warn_changed_keys(state: &mut State) {
    let flagged: Vec<Number> = state.people.iter().filter(|p| p.mismatch).map(|p| p.number).collect();
    state.warned.retain(|n| flagged.contains(n));
    for number in flagged {
        warn(state, number);
    }
}

/// Warns about `number`'s keys, unless done already or their open tab says
/// so (it can only for someone known here, flagged).
pub fn warn(state: &mut State, number: Number) {
    let flagged = state.person(number).is_some_and(|p| p.mismatch);
    let tab_says = flagged && state.tabs.iter().any(|t| t.number() == Some(number));
    if !state.warned.insert(number) || tab_says {
        return;
    }
    let label = state.label(number);
    state.say(Tone::Bad, key_warning(&label));
}

/// A typed number, with the check digit's typo said plainly.
pub fn number(typed: &str) -> Result<Number, String> {
    parse_number(typed).map_err(|e| match e {
        Error::NumberTypo => TYPO.to_string(),
        e => e.to_string(),
    })
}

/// Who `<name|number>` means: a number (spaces and dashes allowed), or a
/// display name as shown, matched without case among the people known here.
pub fn resolve(state: &State, typed: &str) -> Result<Number, String> {
    let typed = typed.trim();
    if typed.chars().any(|c| c.is_ascii_digit()) && typed.chars().all(|c| c.is_ascii_digit() || c == ' ' || c == '-') {
        return number(typed);
    }
    let wanted = typed.to_lowercase();
    let named: Vec<Number> = state
        .people
        .iter()
        .filter(|p| p.name.as_deref().is_some_and(|name| clean(name).trim().to_lowercase() == wanted))
        .map(|p| p.number)
        .collect();
    match named[..] {
        [number] => Ok(number),
        [] => Err(format!("No one here is called {typed}.")),
        _ => Err(format!("More than one person is called {typed}. Use their number.")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::files::username;
    use crate::ui::runner::testing::{App, PASSWORD, TestServer};
    use crate::ui::state::{Line, Tone};
    use crate::ui::update::{Msg, update};
    use crate::ui::view::render;
    use crate::ui::window::{COMPARE, Overlay, rows};
    use corvo_chat_core::{Mark, PersonRow, Secrets};
    use corvo_chat_proto::chat::Relation;
    use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

    fn num(body: u32) -> Number {
        Number::from_body(body).unwrap()
    }

    fn person(number: Number, name: &str, relation: Relation) -> PersonRow {
        PersonRow { number, name: Some(name.into()), relation, online: false, mismatch: false, deleted: false }
    }

    fn relation(state: &State, number: Number) -> Option<Relation> {
        state.person(number).map(|p| p.relation)
    }

    fn mark_of(state: &State, number: Number, text: &str) -> Option<Mark> {
        let chat = state.chats.get(&number)?;
        chat.messages.iter().find(|m| m.text.as_deref() == Ok(text)).and_then(|m| m.mark.clone())
    }

    fn screen_of(state: &State, w: u16, h: u16) -> String {
        let buf = render(state, w, h);
        (0..h).map(|y| (0..w).map(|x| buf[(x, y)].symbol()).collect::<String>()).collect::<Vec<_>>().join("\n")
    }

    /// The sidebar's rows on an 80-column screen: its first 20 columns.
    fn sidebar(screen: &str) -> Vec<String> {
        screen.lines().skip(1).map(|r| r.chars().take(19).collect::<String>().trim_end().to_string()).collect()
    }

    fn row_with(screen: &str, text: &str) -> String {
        screen.lines().find(|r| r.contains(text)).unwrap_or_else(|| panic!("{text:?} not on screen:\n{screen}")).into()
    }

    fn corvo(state: &State) -> Vec<&str> {
        state.tabs[0].lines.iter().map(|l| l.text.as_str()).collect()
    }

    fn last_corvo(state: &State) -> &Line {
        state.tabs[0].lines.last().expect("the corvo tab has lines")
    }

    fn calls(effects: &[Effect]) -> Vec<&Command> {
        effects.iter().filter_map(|e| if let Effect::Call(c) = e { Some(c) } else { None }).collect()
    }

    fn send(state: &mut State, line: &str) -> Vec<Effect> {
        for c in line.chars() {
            update(state, Msg::Key(KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE)));
        }
        update(state, Msg::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)))
    }

    /// Ana knows rahul (a contact), bob (a request), eve (blocked) and an
    /// asked number with no name.
    fn ana() -> (State, [Number; 4]) {
        let mut state = State::new("ana".into(), "482 910 5572".into());
        state.me = Some(num(482_910_557));
        let (rahul, bob, eve, asked) = (num(123_456_789), num(222_333_444), num(555_666_777), num(888_999_000));
        state.people = vec![
            PersonRow { online: true, ..person(rahul, "rahul", Relation::Contact) },
            person(bob, "bob", Relation::Request),
            person(eve, "eve", Relation::Blocked),
            PersonRow { name: None, ..person(asked, "", Relation::Asked) },
        ];
        (state, [rahul, bob, eve, asked])
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_request_can_be_accepted_by_number_or_from_its_tab() {
        let server = TestServer::start().await;
        let data = tempfile::tempdir().unwrap();
        let mut alice = App::active(&data.path().join("alice"), &server, "alice").await;
        let mut carol = App::active(&data.path().join("carol"), &server, "carol").await;
        let (a, c) = (alice.number(), carol.number());
        carol.send(&format!("/add {}", a.digits())).await;
        carol.send("hi alice").await;
        alice.until("carol's request", |s| relation(s, c) == Some(Relation::Request)).await;
        let screen = alice.screen(80, 24);
        assert!(sidebar(&screen).iter().any(|r| r == "Requests (1)"), "{screen}");

        alice.send(&format!("/chat {}", c.digits())).await;
        assert_eq!(alice.state.open_number(), Some(c));
        alice.send("/accept").await;
        alice.until("carol as a contact", |s| relation(s, c) == Some(Relation::Contact)).await;
        assert!(corvo(&alice.state).contains(&"carol is now a contact."), "{:?}", corvo(&alice.state));
        assert!(!alice.screen(80, 24).contains("Requests ("));
        carol.until("alice as a contact", |s| relation(s, a) == Some(Relation::Contact)).await;

        let mut dave = App::active(&data.path().join("dave"), &server, "dave").await;
        let d = dave.number();
        dave.send(&format!("/add {}", a.digits())).await;
        dave.send("hi from dave").await;
        alice.until("dave's request", |s| relation(s, d) == Some(Relation::Request)).await;
        alice.state.select_tab(0);
        alice.send(&format!("/accept {d}")).await;
        alice.until("dave as a contact", |s| relation(s, d) == Some(Relation::Contact)).await;
        for app in [alice, carol, dave] {
            app.close().await;
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn reject_drops_the_request_silently() {
        let server = TestServer::start().await;
        let data = tempfile::tempdir().unwrap();
        let mut alice = App::active(&data.path().join("alice"), &server, "alice").await;
        let mut carol = App::active(&data.path().join("carol"), &server, "carol").await;
        let (a, c) = (alice.number(), carol.number());
        carol.send(&format!("/add {}", a.digits())).await;
        carol.send("hi alice").await;
        alice.until("carol's request", |s| relation(s, c) == Some(Relation::Request)).await;

        alice.send("/reject carol").await;
        alice.until("carol gone", |s| relation(s, c) != Some(Relation::Request)).await;
        let screen = alice.screen(80, 24);
        assert!(sidebar(&screen).iter().all(|r| !r.contains("carol") && !r.contains("Requests")), "{screen}");

        // She isn't told: her next message is just a new request.
        carol.send("still there?").await;
        alice.until("a new request", |s| relation(s, c) == Some(Relation::Request)).await;
        carol.until("both shown", |s| mark_of(s, a, "still there?").is_some()).await;
        assert_eq!(mark_of(&carol.state, a, "hi alice"), Some(Mark::Sent), "it stays ·");
        alice.close().await;
        carol.close().await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_key_change_shows_a_warning_and_blocks_sending() {
        let server = TestServer::start().await;
        let data = tempfile::tempdir().unwrap();
        let mut alice = App::active(&data.path().join("alice"), &server, "alice").await;
        let bob = App::invited(&data.path().join("bob"), &server, "bob", &mut alice).await;
        let (a, b) = (alice.number(), bob.number());
        alice.until("bob as a contact", |s| relation(s, b) == Some(Relation::Contact)).await;
        alice.send("/chat bob").await;
        alice.send("hi").await;
        // The pin has to reach the server for a new device to see it.
        let db = server.state.db.clone();
        let pinned = move || {
            let db = db.clone();
            async move {
                db.call(move |c| {
                    c.query_row(
                        "SELECT COUNT(*) FROM pins JOIN accounts ON accounts.id = pins.account_id \
                         WHERE accounts.number = ? AND pins.number = ?",
                        (a.digits(), b.digits()),
                        |r| r.get::<_, i64>(0),
                    )
                    .unwrap()
                })
                .await
            }
        };
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(20);
        while pinned().await == 0 {
            assert!(tokio::time::Instant::now() < deadline, "the pin never reached the server");
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
        let fresh = Secrets::generate().public_keys();
        server
            .state
            .db
            .call(move |c| {
                c.execute(
                    "UPDATE accounts SET signing_key = ?, exchange_key = ?, key_signature = ? WHERE number = ?",
                    (fresh.signing.0.to_vec(), fresh.exchange.0.to_vec(), fresh.signature.0.to_vec(), b.digits()),
                )
                .unwrap();
            })
            .await;
        let messages = || {
            let db = server.state.db.clone();
            async move {
                db.call(|c| c.query_row("SELECT COUNT(*) FROM messages", [], |r| r.get::<_, i64>(0)).unwrap()).await
            }
        };
        let before = messages().await;

        let mut phone = App::open(&data.path().join("alice-2"), &server);
        let name = username(&alice.dir).expect("alice's username is saved");
        for answer in ["s", name.as_str(), PASSWORD] {
            phone.send(answer).await;
        }
        phone.state.overlay = None;
        phone.until("bob flagged", |s| s.person(b).is_some_and(|p| p.mismatch)).await;
        let warning = "⚠ bob's keys changed. Nothing will be sent to them. Check with /verify.";
        let said = phone.state.tabs[0].lines.iter().find(|l| l.text == warning);
        assert_eq!(said.map(|l| l.tone), Some(Tone::Bad), "{:?}", corvo(&phone.state));
        let screen = phone.screen(80, 24);
        assert!(sidebar(&screen).iter().any(|r| r == "⚠ bob"), "{screen}");

        phone.send("/chat bob").await;
        assert!(phone.screen(100, 24).contains(warning), "at the top of his tab");
        phone.send("are you there?").await;
        let screen = phone.screen(100, 24);
        assert!(row_with(&screen, "are you there?").trim_end().ends_with('✗'), "{screen}");
        assert!(screen.contains("✗ not sent: Their keys changed. Check with /verify."), "{screen}");
        assert!(phone.state.editor.is_empty(), "the message is shown, not given back");
        assert_eq!(messages().await, before, "nothing reached the server");
        for app in [alice, bob, phone] {
            app.close().await;
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn verify_shows_the_same_code_on_both_sides() {
        let server = TestServer::start().await;
        let data = tempfile::tempdir().unwrap();
        let mut alice = App::active(&data.path().join("alice"), &server, "alice").await;
        let mut bob = App::invited(&data.path().join("bob"), &server, "bob", &mut alice).await;
        let (a, b) = (alice.number(), bob.number());
        alice.until("bob as a contact", |s| s.person(b).is_some()).await;
        bob.until("alice as a contact", |s| s.person(a).is_some()).await;
        // Writing to someone pins their keys.
        for (app, other) in [(&mut alice, "bob"), (&mut bob, "alice")] {
            app.send(&format!("/chat {other}")).await;
            app.send("hello").await;
        }

        let mut codes = Vec::new();
        for (app, other) in [(&mut alice, "bob"), (&mut bob, "alice")] {
            app.send(&format!("/verify {other}")).await;
            let Some(overlay @ Overlay::SafetyCode { .. }) = app.state.overlay.clone() else {
                panic!("no safety code window: {:?}", corvo(&app.state))
            };
            let shown = rows(&overlay, (80, 24));
            let groups: Vec<&str> = shown[..3].iter().flat_map(|r| r.split(' ')).collect();
            assert_eq!(groups.len(), 12, "{shown:#?}");
            assert!(shown[..3].iter().all(|r| r.split(' ').count() == 4), "3 rows of 4: {shown:#?}");
            assert!(groups.iter().all(|g| g.len() == 5 && g.chars().all(|c| c.is_ascii_digit())), "{shown:#?}");
            let screen = app.screen(80, 24);
            assert!(screen.contains(&format!("Safety code for {other}")), "{screen}");
            assert!(screen.contains("Compare it with them in person or on a call."), "{screen}");
            assert_eq!(shown[4..].join(" "), COMPARE);
            codes.push(shown[..3].join(" "));
            app.press(KeyCode::Esc).await;
            assert_eq!(app.state.overlay, None);
        }
        assert_eq!(codes[0], codes[1], "the same on both sides");
        alice.close().await;
        bob.close().await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn invite_shows_a_code_and_lists_open_invites() {
        let server = TestServer::start().await;
        let data = tempfile::tempdir().unwrap();
        let mut alice = App::active(&data.path().join("alice"), &server, "alice").await;
        let mut carol = App::pending(&data.path().join("carol"), &server, "carol").await;
        let c = carol.number();

        alice.send("/invite 482 910 5574").await;
        assert_eq!(last_corvo(&alice.state).text, TYPO);
        alice.send(&format!("/invite {}", alice.number())).await;
        assert_eq!(last_corvo(&alice.state).text, "That's your own number.");

        alice.send(&format!("/invite {c}")).await;
        let said = last_corvo(&alice.state).text.clone();
        let rest = said.strip_prefix("Invite code for carol: ").unwrap_or_else(|| panic!("{said:?}"));
        let (code, rest) = rest.split_at(7);
        assert!(code.as_bytes()[3] == b' ' && code.replace(' ', "").chars().all(|c| c.is_ascii_digit()), "{said:?}");
        assert!(rest.starts_with(". Send it to them. It works until ") && rest.ends_with('.'), "{said:?}");

        alice.send("/invite").await;
        let lines = corvo(&alice.state);
        let head = lines.iter().rposition(|l| l.starts_with("NAME")).unwrap_or_else(|| panic!("{lines:?}"));
        assert!(lines[head].contains("NUMBER") && lines[head].contains("CODE"), "{lines:?}");
        let row = lines[head + 1];
        assert!(row.starts_with("carol") && row.contains(&c.to_string()) && row.contains(code), "{lines:?}");

        // The code works.
        carol.send(&code.replace(' ', "")).await;
        assert!(matches!(carol.state.screen, crate::ui::state::Screen::Main), "carol is in");
        alice.until("carol as a contact", |s| relation(s, c) == Some(Relation::Contact)).await;
        alice.send("/invite").await;
        assert_eq!(last_corvo(&alice.state).text, "No open invites. /invite <number> makes one.");
        alice.close().await;
        carol.close().await;
    }

    #[test]
    fn names_resolve_and_ambiguous_names_say_so() {
        let (mut state, [rahul, bob, ..]) = ana();
        assert_eq!(resolve(&state, "rahul"), Ok(rahul));
        assert_eq!(resolve(&state, " BOB "), Ok(bob), "any case");
        assert_eq!(resolve(&state, &rahul.to_string()), Ok(rahul), "a number with spaces");
        assert_eq!(resolve(&state, &rahul.digits()), Ok(rahul));
        assert_eq!(resolve(&state, "123 456 7890"), Err(TYPO.into()));
        assert_eq!(resolve(&state, "nobody"), Err("No one here is called nobody.".into()));
        state.people.push(person(num(111_222_333), "Sam", Relation::Contact));
        state.people.push(person(num(444_555_666), "sam", Relation::Request));
        assert_eq!(resolve(&state, "Sam"), Err("More than one person is called Sam. Use their number.".into()));
        assert_eq!(resolve(&state, &num(444_555_666).to_string()), Ok(num(444_555_666)));
    }

    #[test]
    fn accept_reject_and_block_take_this_tab_or_a_name() {
        let (mut state, [_, bob, eve, _]) = ana();
        assert!(calls(&send(&mut state, "/accept")).is_empty());
        assert_eq!(last_corvo(&state).text, "Which request? /accept <name|number>, or open their chat first.");
        state.open_chat(bob, true);
        assert_eq!(calls(&send(&mut state, "/accept")), [&Command::Accept { number: bob }]);
        assert_eq!(calls(&send(&mut state, "/reject")), [&Command::Reject { number: bob }]);
        assert_eq!(calls(&send(&mut state, "/block")), [&Command::Block { number: bob }]);
        assert_eq!(calls(&send(&mut state, "/accept rahul")), Vec::<&Command>::new());
        assert_eq!(last_corvo(&state).text, "rahul is already a contact.");
        send(&mut state, "/reject eve");
        assert_eq!(last_corvo(&state).text, "You've blocked eve: /unblock them first.");
        send(&mut state, "/unblock rahul");
        assert_eq!(last_corvo(&state).text, "rahul isn't blocked.");
        assert_eq!(calls(&send(&mut state, "/unblock eve")), [&Command::Unblock { number: eve }]);
        send(&mut state, "/block eve");
        assert_eq!(last_corvo(&state).text, "eve is already blocked.");
        send(&mut state, "/block 482 910 5572");
        assert_eq!(last_corvo(&state).text, "That's your own number.");
        let stranger = num(999_000_111);
        assert_eq!(calls(&send(&mut state, &format!("/block {stranger}"))), [&Command::Block { number: stranger }]);
    }

    #[test]
    fn remove_asks_whether_to_delete_the_history_too() {
        let (mut state, [rahul, ..]) = ana();
        send(&mut state, "/remove bob");
        assert_eq!(last_corvo(&state).text, "bob is a request: /reject drops it.");
        assert!(send(&mut state, "/remove rahul").is_empty());
        assert_eq!(state.ask().map(|a| a.label), Some("Delete the history too? y/n: ".into()));
        let screen = screen_of(&state, 80, 24);
        assert!(screen.contains("Delete the history too? y/n:"), "{screen}");
        send(&mut state, "maybe");
        assert_eq!(last_corvo(&state).text, "Type y or n.");
        assert!(state.pending.is_some(), "still asking");
        let effects = send(&mut state, "N");
        assert_eq!(calls(&effects), [&Command::Remove { number: rahul, clear_history: false }]);
        assert_eq!(state.pending, None);
        send(&mut state, "/remove rahul");
        assert_eq!(calls(&send(&mut state, "yes")), [&Command::Remove { number: rahul, clear_history: true }]);
        send(&mut state, "/remove rahul");
        update(&mut state, Msg::Key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE)));
        assert_eq!(state.pending, None);
        assert_eq!(last_corvo(&state).text, "Cancelled.");
    }

    #[test]
    fn report_asks_a_reason_then_whether_to_block() {
        let (mut state, [rahul, ..]) = ana();
        assert!(send(&mut state, "/report rahul").is_empty());
        assert_eq!(state.ask().map(|a| a.label), Some("Reason (optional, Enter to skip): ".into()));
        let long = "spam ".repeat(60);
        assert!(send(&mut state, &long).is_empty());
        assert_eq!(state.ask().map(|a| a.label), Some("Block them too? y/n: ".into()));
        let effects = send(&mut state, "y");
        let reason: String = long.chars().take(200).collect();
        let reason = reason.trim().to_string();
        assert_eq!(calls(&effects), [&Command::Report { number: rahul, reason: Some(reason), block: true }]);
        assert_eq!(state.history.prev("").as_deref(), Some("/report rahul"), "answers aren't kept");

        send(&mut state, "/report rahul");
        send(&mut state, "");
        assert_eq!(calls(&send(&mut state, "n")), [&Command::Report { number: rahul, reason: None, block: false }]);
    }

    #[test]
    fn tables_list_contacts_requests_blocked_and_info() {
        let (mut state, [rahul, bob, eve, asked]) = ana();
        state.open_chat(rahul, true);
        send(&mut state, "/contacts");
        assert!(state.on_corvo(), "lists go to the corvo tab");
        let lines = corvo(&state);
        let at = lines.iter().position(|l| l.starts_with("NAME")).expect("a table");
        assert_eq!(lines[at], "NAME   STATUS    NUMBER");
        assert_eq!(lines[at + 1], format!("rahul  ● online  {rahul}"));
        assert!(lines.iter().any(|l| l.starts_with("REQUESTS") && l.contains("NUMBER")), "{lines:?}");
        assert!(lines.iter().any(|l| l.starts_with("bob") && l.contains(&bob.to_string())), "{lines:?}");
        send(&mut state, "/blocked");
        let lines = corvo(&state);
        assert_eq!(lines[lines.len() - 2], format!("eve   {eve}"));
        send(&mut state, "/requests");
        assert!(corvo(&state).iter().any(|l| *l == format!("bob   0       {bob}")), "{:?}", corvo(&state));

        send(&mut state, "/info rahul");
        let Some(overlay @ Overlay::Info { .. }) = state.overlay.clone() else { panic!("{:?}", state.overlay) };
        let shown = rows(&overlay, (80, 24));
        assert_eq!(shown, ["Name:      rahul", &format!("Number:    {rahul}"), "Relation:  contact", "Online:    yes"]);
        state.overlay = None;
        send(&mut state, &format!("/info {asked}"));
        let Some(overlay @ Overlay::Info { .. }) = state.overlay.clone() else { panic!("{:?}", state.overlay) };
        let shown = rows(&overlay, (80, 24));
        assert_eq!(shown[0], "Name:      (not shared)");
        assert!(!shown.iter().any(|r| r.starts_with("Online")), "online is for contacts only: {shown:?}");
    }

    #[test]
    fn hostile_names_are_cleaned_in_tables_and_windows() {
        let (mut state, [rahul, ..]) = ana();
        state.people[0].name = Some("ev\x1b[2Jil\u{202e}".into());
        send(&mut state, "/contacts");
        assert!(corvo(&state).iter().any(|l| l.starts_with("evil ")), "{:?}", corvo(&state));
        send(&mut state, &format!("/info {rahul}"));
        let screen = screen_of(&state, 80, 24);
        assert!(!screen.contains('\x1b') && !screen.contains('\u{202e}'), "{screen}");
        assert!(screen.contains("Name:      evil"), "{screen}");
        state.overlay = None;
        let verify = Command::Verify { number: rahul };
        update(
            &mut state,
            Msg::Done(crate::ui::update::Outcome::Reply {
                asked: verify,
                reply: Ok(Reply::SafetyCode("11111 ".repeat(12).trim().into())),
            }),
        );
        let screen = screen_of(&state, 80, 24);
        assert!(screen.contains("Safety code for evil "), "{screen}");
    }
}
