//! Chatting: the chat list and the people for the sidebar, each open tab's
//! messages, sending, older pages and read marks. All of it is asked of the
//! core with [`Effect::Call`]; the answers come back to [`reply`], and the
//! core's own news to [`on_update`].

use std::collections::HashSet;

use chrono::{DateTime, Local};
use corvo_chat_core::native::{Command, Reply};
use corvo_chat_core::{ChatRow, ConnState, Error, MsgId, PersonRow, ShownMessage, Update, parse_number};
use corvo_chat_proto::Number;
use corvo_chat_proto::chat::Relation;

use super::state::{APP, Chat, Screen, State, Tone};
use super::update::Effect;
use super::view::{areas, chat_page, clean, input_rows};

/// Messages loaded at a time: a tab's newest, then each older page.
pub const PAGE: usize = 50;
/// What a number with a wrong check digit gets.
pub const TYPO: &str = "That number has a typo";

/// Something loaded from the core that a change can make out of date.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Want {
    Chats,
    People,
    /// A chat's newest messages.
    Messages([u8; 16]),
}

/// What's been asked of the core and not answered yet, and what changed
/// meanwhile. A burst of updates asks once, then once more at the end.
#[derive(Debug, Default)]
pub struct Loads {
    asked: HashSet<Want>,
    stale: HashSet<Want>,
}

/// What the core says changed. Nothing is loaded before the main screen.
pub fn on_update(state: &mut State, update: Update) -> Vec<Effect> {
    let mut effects = Vec::new();
    if let Update::Connection(conn) = update {
        state.conn = Some(conn);
    }
    if !matches!(state.screen, Screen::Main) {
        return effects;
    }
    match update {
        Update::Connection(ConnState::Online) => {
            // A mark that failed offline goes again.
            for chat in state.chats.values_mut() {
                chat.marked = None;
            }
            reload_all(state, &mut effects);
        }
        Update::Connection(_) => {}
        Update::Synced => reload_all(state, &mut effects),
        Update::Message { chat_id, .. }
        | Update::Marks { chat_id }
        | Update::Read { chat_id }
        | Update::Sent { chat_id, .. } => changed(state, chat_id, &mut effects),
        Update::Chat { chat_id } => {
            // Older messages may have come, or the chat was cleared.
            if let Some((_, chat)) = chat_by_id(state, chat_id) {
                chat.more = true;
            }
            changed(state, chat_id, &mut effects);
        }
        Update::Refused { chat_id, message, .. } => {
            changed(state, chat_id, &mut effects);
            let to = state.list.iter().find(|r| r.chat_id == chat_id).map(|r| r.with);
            let to = to.map_or_else(String::new, |n| format!(" to {}", state.label(n)));
            state.say(Tone::Bad, format!("A message{to} wasn't sent: {message}"));
        }
        Update::Person { .. } | Update::KeyMismatch { .. } => {
            want(state, Want::People, &mut effects);
            want(state, Want::Chats, &mut effects);
        }
        Update::Presence { .. } => want(state, Want::People, &mut effects),
        Update::Profile => effects.push(Effect::Call(Command::Account)),
        Update::Devices | Update::SignedOut(_) => {}
    }
    effects
}

/// Loads everything again: the chat list, the people and the open chats.
pub fn reload_all(state: &mut State, effects: &mut Vec<Effect>) {
    want(state, Want::Chats, effects);
    want(state, Want::People, effects);
    let open: Vec<[u8; 16]> = state.chats.values().filter_map(|c| c.chat_id).collect();
    for chat_id in open {
        want(state, Want::Messages(chat_id), effects);
    }
}

/// A chat changed: the list, and the chat if a tab shows it.
fn changed(state: &mut State, chat_id: [u8; 16], effects: &mut Vec<Effect>) {
    want(state, Want::Chats, effects);
    if chat_by_id(state, chat_id).is_some() {
        want(state, Want::Messages(chat_id), effects);
    }
}

/// Asks for `what`, unless it's already asked: then once more after that answer.
fn want(state: &mut State, what: Want, effects: &mut Vec<Effect>) {
    if !state.loads.asked.insert(what) {
        state.loads.stale.insert(what);
        return;
    }
    let command = match what {
        Want::Chats => Command::Chats,
        Want::People => Command::People,
        Want::Messages(chat_id) => {
            // As many as shown and a page more, so new ones never push shown ones out.
            let held = match chat_by_id(state, chat_id) {
                Some((_, chat)) => {
                    chat.asked_at = chat.pages;
                    chat.messages.len()
                }
                None => 0,
            };
            Command::Messages { chat_id, before: None, limit: held + PAGE }
        }
    };
    effects.push(Effect::Call(command));
}

/// `what` was answered: asks again if it changed meanwhile.
fn answered(state: &mut State, what: Want, effects: &mut Vec<Effect>) {
    state.loads.asked.remove(&what);
    if state.loads.stale.remove(&what) {
        want(state, what, effects);
    }
}

/// The open tab with chat `chat_id`, and who it's with.
fn chat_by_id(state: &mut State, chat_id: [u8; 16]) -> Option<(Number, &mut Chat)> {
    state.chats.iter_mut().find(|(_, c)| c.chat_id == Some(chat_id)).map(|(n, c)| (*n, c))
}

/// The core's answer to `asked`.
pub fn reply(state: &mut State, asked: Command, reply: Result<Reply, String>, effects: &mut Vec<Effect>) {
    match (asked, reply) {
        (Command::Chats, reply) => {
            answered(state, Want::Chats, effects);
            match reply {
                Ok(Reply::Chats(rows)) => set_list(state, rows, effects),
                Ok(_) => {}
                Err(why) => state.say(Tone::Bad, format!("Couldn't read your chats: {why}")),
            }
        }
        (Command::People, reply) => {
            answered(state, Want::People, effects);
            match reply {
                Ok(Reply::People(people)) => set_people(state, people),
                Ok(_) => {}
                Err(why) => state.say(Tone::Bad, format!("Couldn't read who you know: {why}")),
            }
        }
        (Command::Messages { chat_id, before: None, .. }, reply) => {
            // Asked before an older page came: its limit can't reach that page,
            // so it's asked again instead.
            let behind = chat_by_id(state, chat_id).is_some_and(|(_, c)| c.asked_at != c.pages);
            if behind {
                state.loads.stale.insert(Want::Messages(chat_id));
            }
            answered(state, Want::Messages(chat_id), effects);
            match reply {
                _ if behind => {}
                Ok(Reply::Messages(messages)) => newest(state, chat_id, messages),
                Ok(_) => {}
                Err(why) => state.say(Tone::Bad, format!("Couldn't read that chat: {why}")),
            }
        }
        (Command::Messages { chat_id, before: Some(_), .. }, reply) => older(state, chat_id, reply),
        // Whether or not the server had more (offline it can't say), what's saved comes next.
        (Command::Older { chat_id }, reply) => {
            if let Some((_, chat)) = chat_by_id(state, chat_id) {
                chat.complete = matches!(reply, Ok(Reply::Complete(true)));
            }
            older_from_cache(state, chat_id, effects);
        }
        (Command::SendText { to, .. }, Ok(_)) => {
            want(state, Want::Chats, effects);
            if let Some(chat_id) = state.chats.get(&to).and_then(|c| c.chat_id) {
                want(state, Want::Messages(chat_id), effects);
            }
        }
        (Command::SendText { text, .. }, Err(why)) => {
            state.say(Tone::Bad, why);
            // Nothing typed is lost.
            if state.editor.is_empty() {
                state.editor.set(text);
            }
        }
        // Offline it fails; it goes again once online.
        (Command::MarkRead { .. }, _) => want(state, Want::Chats, effects),
        (Command::Add { number }, Ok(_)) => {
            want(state, Want::People, effects);
            open(state, number, true, effects);
            let label = state.label(number);
            state.say(Tone::Good, format!("{label} added: what you type in their tab goes to them."));
        }
        (Command::Account, Ok(Reply::Account(account))) => {
            state.name = account.display_name;
            state.number = account.number.to_string();
        }
        (_, Err(why)) => state.say(Tone::Bad, why),
        (_, Ok(_)) => {}
    }
}

/// A new chat list. A tab whose chat appeared (its first message) or moved
/// (it lost to one made at the same time) loads it.
fn set_list(state: &mut State, rows: Vec<ChatRow>, effects: &mut Vec<Effect>) {
    state.list = rows;
    let moved: Vec<(Number, [u8; 16])> = state
        .chats
        .iter()
        .filter_map(|(number, chat)| {
            let row = state.list.iter().find(|r| r.with == *number)?;
            (chat.chat_id != Some(row.chat_id)).then_some((*number, row.chat_id))
        })
        .collect();
    for (number, chat_id) in moved {
        if let Some(chat) = state.chats.get_mut(&number) {
            let cleared = chat.cleared;
            *chat = Chat { cleared, ..Chat::new(Some(chat_id)) };
        }
        want(state, Want::Messages(chat_id), effects);
    }
    state.relabel();
}

fn set_people(state: &mut State, people: Vec<PersonRow>) {
    state.people = people;
    state.relabel();
}

/// A chat's newest messages. Once a tab has loaded, it keeps showing from
/// its oldest message on, older pages included.
fn newest(state: &mut State, chat_id: [u8; 16], messages: Vec<ShownMessage>) {
    let Some((_, chat)) = chat_by_id(state, chat_id) else { return };
    let floor = if chat.loaded { chat.oldest() } else { None };
    chat.messages =
        messages.into_iter().filter(|m| floor.is_none_or(|floor| m.seq.is_none_or(|seq| seq >= floor))).collect();
    chat.loaded = true;
    state.changes += 1;
}

/// Asks for older messages when the open chat is scrolled to its top and
/// there may be some.
pub fn load_older(state: &mut State, effects: &mut Vec<Effect>) {
    let Some(number) = state.open_number() else { return };
    if !state.scroll.is_scrolled() {
        return;
    }
    let Some(chat) = state.chats.get(&number) else { return };
    let Some(chat_id) = chat.chat_id.filter(|_| chat.more && !chat.loading && chat.loaded) else { return };
    let (total, height) = chat_page(state);
    if state.scroll.top(total, height) > 0 {
        return;
    }
    if let Some(chat) = state.chats.get_mut(&number) {
        chat.loading = true;
    }
    effects.push(Effect::Call(Command::Older { chat_id }));
}

/// After the server was asked for older messages: the page before the
/// oldest shown, from what's saved.
fn older_from_cache(state: &mut State, chat_id: [u8; 16], effects: &mut Vec<Effect>) {
    let Some((_, chat)) = chat_by_id(state, chat_id) else { return };
    match chat.oldest() {
        Some(before) => effects.push(Effect::Call(Command::Messages { chat_id, before: Some(before), limit: PAGE })),
        None => {
            chat.loading = false;
            chat.more = false;
        }
    }
}

/// An older page goes before what's shown, without moving what's on screen.
/// A page that brings nothing new isn't asked for again: the view would
/// still be at the top, and would ask at once, forever. Nor is one when the
/// server said it has nothing older and the page is the last that's saved.
fn older(state: &mut State, chat_id: [u8; 16], reply: Result<Reply, String>) {
    let open = state.open_number();
    let Some((number, chat)) = chat_by_id(state, chat_id) else { return };
    chat.loading = false;
    let page = match reply {
        Ok(Reply::Messages(page)) => page,
        Ok(_) => Vec::new(),
        Err(why) => {
            chat.more = false;
            return state.say(Tone::Bad, format!("Couldn't read older messages: {why}"));
        }
    };
    let last_saved = page.len() < PAGE;
    let have: HashSet<MsgId> = chat.messages.iter().map(|m| m.msg_id).collect();
    let page: Vec<ShownMessage> = page.into_iter().filter(|m| !have.contains(&m.msg_id)).collect();
    chat.more = !page.is_empty() && !(chat.complete && last_saved);
    if page.is_empty() {
        return;
    }
    chat.pages += 1;
    let shown = open == Some(number);
    let before = if shown { chat_page(state).0 } else { 0 };
    if let Some(chat) = state.chats.get_mut(&number) {
        chat.messages.splice(0..0, page);
    }
    state.changes += 1;
    if shown {
        let after = chat_page(state).0;
        state.scroll.shift(after.saturating_sub(before));
    }
}

/// Marks the open chat read when new messages of theirs are on screen: the
/// window has focus, it's big enough to draw, the chat is scrolled to its
/// end and nothing covers it.
pub fn read_open_chat(state: &mut State, effects: &mut Vec<Effect>) {
    let visible = state.focused
        && matches!(state.screen, Screen::Main)
        && areas(state.size, input_rows(state)).is_some()
        && !state.scroll.is_scrolled()
        && state.overlay.is_none();
    let Some(number) = state.open_number().filter(|_| visible) else { return };
    let Some(chat) = state.chats.get(&number) else { return };
    let Some(chat_id) = chat.chat_id else { return };
    let unread = state.list.iter().any(|r| r.chat_id == chat_id && r.unread > 0);
    let newest = chat.messages.iter().filter(|m| !m.mine).filter_map(|m| m.seq).max();
    if unread && newest > chat.marked {
        if let Some(chat) = state.chats.get_mut(&number) {
            chat.marked = newest;
        }
        effects.push(Effect::Call(Command::MarkRead { chat_id }));
    }
}

/// The window's title: the unread count, then the app's name.
pub fn title(state: &State) -> String {
    match state.unread_total() {
        0 => APP.to_string(),
        n => format!("({n}) {APP}"),
    }
}

/// The tab for a chat with `number`, opened if needed (loading it), and
/// selected if `switch`.
pub fn open(state: &mut State, number: Number, switch: bool, effects: &mut Vec<Effect>) {
    if !state.open_chat(number, switch) {
        return;
    }
    if let Some(row) = state.list.iter().find(|r| r.with == number) {
        let chat_id = row.chat_id;
        if let Some(chat) = state.chats.get_mut(&number) {
            chat.chat_id = Some(chat_id);
        }
        want(state, Want::Messages(chat_id), effects);
    }
}

/// A line typed in a chat tab. Offline it waits, `○`, and goes once online.
pub fn send(state: &mut State, to: Number, text: String, effects: &mut Vec<Effect>) {
    if state.person(to).is_some_and(|p| p.relation == Relation::Blocked) {
        let label = state.label(to);
        return state.say(Tone::Bad, format!("You've blocked {label}: /unblock them first"));
    }
    effects.push(Effect::Call(Command::SendText { to, text }));
}

/// `/add <number>`: looks them up, then opens a tab for them.
pub fn add(state: &mut State, typed: &str, effects: &mut Vec<Effect>) {
    match number(typed) {
        Err(why) => state.say(Tone::Bad, why),
        Ok(number) if Some(number) == state.me => state.say(Tone::Bad, "That's your own number."),
        Ok(number) => effects.push(Effect::Call(Command::Add { number })),
    }
}

/// `/chat <name|number>`: opens the chat with someone known here.
pub fn open_named(state: &mut State, who: &str, effects: &mut Vec<Effect>) {
    let number = match resolve(state, who) {
        Ok(number) => number,
        Err(why) => return state.say(Tone::Bad, why),
    };
    let known = state.person(number).is_some() || state.list.iter().any(|r| r.with == number);
    if known || state.chats.contains_key(&number) {
        open(state, number, true, effects);
    } else {
        state.say(Tone::Bad, format!("{number} isn't in your chats yet: /add {number} first"));
    }
}

/// A typed number, with the check digit's typo said plainly.
fn number(typed: &str) -> Result<Number, String> {
    parse_number(typed).map_err(|e| match e {
        Error::NumberTypo => TYPO.to_string(),
        e => e.to_string(),
    })
}

/// Who `<name|number>` means: a number (spaces allowed), or a display name
/// as shown, matched without case among the people known here.
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

/// A time in Unix milliseconds in the local time zone.
pub fn local(ms: u64) -> Option<DateTime<Local>> {
    DateTime::from_timestamp_millis(i64::try_from(ms).ok()?).map(|t| t.with_timezone(&Local))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ui::runner::testing::{App, Proxy, TestServer};
    use crate::ui::state::Line;
    use crate::ui::update::{Msg, Outcome, update};
    use crate::ui::view::render;
    use corvo_chat_core::Mark;
    use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

    /// A valid number from its nine-digit body.
    fn num(body: u32) -> Number {
        Number::from_body(body).unwrap()
    }

    /// The open chats' messages with `number`: (text, mark), oldest first.
    fn shown(state: &State, number: Number) -> Vec<(String, Option<Mark>)> {
        let messages = state.chats.get(&number).map_or(&[][..], |c| &c.messages[..]);
        messages.iter().map(|m| (m.text.clone().unwrap_or_default(), m.mark.clone())).collect()
    }

    fn mark_of(state: &State, number: Number, text: &str) -> Option<Mark> {
        shown(state, number).into_iter().find(|(t, _)| t == text).and_then(|(_, mark)| mark)
    }

    /// The screen's row holding `text`.
    fn row_with(screen: &str, text: &str) -> String {
        screen.lines().find(|r| r.contains(text)).unwrap_or_else(|| panic!("{text:?} not on screen:\n{screen}")).into()
    }

    fn screen_of(state: &State, w: u16, h: u16) -> String {
        let buf = render(state, w, h);
        (0..h).map(|y| (0..w).map(|x| buf[(x, y)].symbol()).collect::<String>()).collect::<Vec<_>>().join("\n")
    }

    fn last_corvo(state: &State) -> &Line {
        state.tabs[0].lines.last().expect("the corvo tab has lines")
    }

    fn person(number: Number, name: &str, relation: Relation) -> PersonRow {
        PersonRow { number, name: Some(name.into()), relation, online: false, mismatch: false, deleted: false }
    }

    fn message(seq: i64, mine: bool, text: &str, mark: Option<Mark>) -> ShownMessage {
        ShownMessage {
            chat_id: [7; 16],
            seq: Some(seq),
            msg_id: MsgId([seq as u8; 16]),
            mine,
            from: num(111_111_111),
            text: Ok(text.into()),
            sent_at_ms: 1_760_000_000_000,
            mark,
        }
    }

    /// Ana on the main screen, with rahul (a contact) in an open tab.
    fn ana_and_rahul() -> (State, Number) {
        let mut state = State::new("ana".into(), "482 910 5572".into());
        state.me = Some(num(482_910_557));
        let rahul = num(123_456_789);
        state.people = vec![person(rahul, "rahul", Relation::Contact)];
        state.open_chat(rahul, true);
        state.chats.get_mut(&rahul).unwrap().chat_id = Some([7; 16]);
        (state, rahul)
    }

    fn calls(effects: &[Effect]) -> Vec<&Command> {
        effects.iter().filter_map(|e| if let Effect::Call(c) = e { Some(c) } else { None }).collect()
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn two_people_chat_and_marks_move() {
        let server = TestServer::start().await;
        let data = tempfile::tempdir().unwrap();
        let mut alice = App::active(&data.path().join("alice"), &server, "alice").await;
        let bob = App::invited(&data.path().join("bob"), &server, "bob", &mut alice).await;
        let (a, b) = (alice.number(), bob.number());
        alice.until("bob as a contact", |s| s.person(b).is_some_and(|p| p.relation == Relation::Contact)).await;
        // Bob's device is off, so nothing of his can have it yet.
        let bob_dir = bob.dir.clone();
        bob.close().await;

        alice.send("/chat bob").await;
        assert_eq!(alice.state.open_number(), Some(b));
        alice.send("hi").await;
        assert_eq!(shown(&alice.state, b), [("hi".to_string(), Some(Mark::Sent))]);
        assert!(row_with(&alice.screen(80, 24), "hi").trim_end().ends_with('·'));

        let mut bob = App::unlock(&bob_dir, server.server.clone()).await;
        alice.until("✓", |s| mark_of(s, b, "hi") == Some(Mark::Received)).await;
        assert!(row_with(&alice.screen(80, 24), "hi").trim_end().ends_with('✓'));

        bob.until("alice as a contact", |s| s.person(a).is_some()).await;
        bob.send("/chat alice").await;
        bob.until("hi", |s| shown(s, a).iter().any(|(t, _)| t == "hi")).await;
        alice.until("✓✓", |s| mark_of(s, b, "hi") == Some(Mark::Read)).await;

        // Theirs on the left, after the time; yours on the right, with the mark.
        let chat_x = 21;
        let theirs: String = row_with(&bob.screen(80, 24), "hi").chars().skip(chat_x).collect();
        let (time, text) = theirs.trim_end().split_at(6);
        assert!(time.chars().nth(2) == Some(':') && text == "hi", "{theirs:?}");
        let mine: String = row_with(&alice.screen(80, 24), "hi").chars().skip(chat_x).collect();
        assert!(mine.trim_end().ends_with("✓✓") && mine.starts_with(&" ".repeat(30)), "{mine:?}");
        alice.close().await;
        bob.close().await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn restart_offline_shows_chats_and_sends_later() {
        let server = TestServer::start().await;
        let mut proxy = Proxy::start(&server).await;
        let data = tempfile::tempdir().unwrap();
        let (alice_dir, bob_dir) = (data.path().join("alice"), data.path().join("bob"));
        let mut alice = App::active_at(&alice_dir, &server, proxy.server(&server), "alice").await;
        let mut bob = App::invited(&bob_dir, &server, "bob", &mut alice).await;
        let (a, b) = (alice.number(), bob.number());
        bob.until("alice as a contact", |s| s.person(a).is_some()).await;
        bob.send("/chat alice").await;
        bob.send("hello from before").await;
        let last_is = |s: &State, text: &str| {
            s.list.iter().any(|r| r.with == b && r.last.as_ref().is_some_and(|m| m.text.as_deref() == Ok(text)))
        };
        alice.until("bob's message", |s| last_is(s, "hello from before")).await;
        alice.close().await;
        // Bob is away too, so what alice sends stays `·` once it's there.
        bob.close().await;

        proxy.cut().await;
        let mut alice = App::unlock(&alice_dir, proxy.server(&server)).await;
        alice.until("reconnecting", |s| matches!(s.conn, Some(ConnState::Reconnecting { .. }))).await;
        alice.until("the saved chats", |s| s.person(b).is_some() && last_is(s, "hello from before")).await;
        let screen = alice.screen(80, 24);
        assert!(screen.lines().skip(1).any(|r| r.starts_with("○ bob")), "bob in the sidebar:\n{screen}");
        alice.send("/chat bob").await;
        assert!(alice.screen(80, 24).contains("hello from before"), "the history");

        alice.send("are you there?").await;
        assert_eq!(mark_of(&alice.state, b, "are you there?"), Some(Mark::Waiting));
        let screen = alice.screen(80, 24);
        assert!(row_with(&screen, "are you there?").trim_end().ends_with('○'), "{screen}");
        assert!(screen.lines().last().unwrap().contains("● reconnecting in"), "{screen}");
        assert!(screen.lines().last().unwrap().contains("1 waiting"), "{screen}");

        proxy.open();
        alice.until("·", |s| mark_of(s, b, "are you there?") == Some(Mark::Sent)).await;
        assert!(row_with(&alice.screen(80, 24), "are you there?").trim_end().ends_with('·'));

        let mut bob = App::unlock(&bob_dir, server.server.clone()).await;
        bob.until("alice as a contact", |s| s.person(a).is_some()).await;
        bob.send("/chat alice").await;
        bob.until("the message", |s| shown(s, a).iter().any(|(t, _)| t == "are you there?")).await;
        let copies = shown(&bob.state, a).iter().filter(|(t, _)| t == "are you there?").count();
        assert_eq!(copies, 1, "it went once");
        alice.close().await;
        bob.close().await;
    }

    #[test]
    fn escape_codes_from_others_are_drawn_harmless() {
        let (mut state, rahul) = ana_and_rahul();
        let evil = "eve\x1b[2J\u{202e}lyn";
        state.people = vec![person(rahul, evil, Relation::Contact)];
        let mut undecryptable = message(3, false, "", None);
        undecryptable.text = Err("⚠".into());
        state.chats.get_mut(&rahul).unwrap().messages = vec![
            message(1, false, "hi\x1b[2J\u{202e}there", None),
            message(2, true, "me\x1b]0;title\x07 too\u{202e}", Some(Mark::Read)),
            undecryptable,
        ];
        state.relabel();
        // A warning naming them flashes under the chat.
        state.say(Tone::Warn, format!("{evil} says hi"));
        let screen = screen_of(&state, 140, 24);
        for bad in ["\x1b", "\u{202e}", "[2J", "title", "\x07"] {
            assert!(!screen.contains(bad), "{bad:?} drawn:\n{screen}");
        }
        assert!(row_with(&screen, "hithere").contains(":"), "{screen}");
        assert!(screen.lines().next().unwrap().contains(" evelyn "), "the tab title:\n{screen}");
        assert!(screen.lines().skip(1).any(|r| r.starts_with("○ evelyn")), "the sidebar:\n{screen}");
        assert!(screen.contains("me too"), "{screen}");
        assert!(screen.contains("⚠ couldn't decrypt a message from evelyn"), "{screen}");
        assert!(screen.contains("evelyn says hi"), "the flash:\n{screen}");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn older_messages_load_at_the_top() {
        let server = TestServer::start().await;
        let data = tempfile::tempdir().unwrap();
        let mut alice = App::active(&data.path().join("alice"), &server, "alice").await;
        let mut bob = App::invited(&data.path().join("bob"), &server, "bob", &mut alice).await;
        let (a, b) = (alice.number(), bob.number());
        bob.until("alice as a contact", |s| s.person(a).is_some()).await;
        for i in 0..120 {
            bob.call(Command::SendText { to: a, text: format!("m{i:03}") }).await.expect("sent");
        }
        let has_all = |s: &State| {
            s.list.iter().any(|r| r.with == b && r.last.as_ref().is_some_and(|m| m.text.as_deref() == Ok("m119")))
        };
        alice.until("all 120", has_all).await;
        alice.send("/chat bob").await;
        let texts = |app: &App| shown(&app.state, b).into_iter().map(|(t, _)| t).collect::<Vec<_>>();
        assert_eq!(texts(&alice).len(), PAGE, "the newest page");
        assert_eq!(texts(&alice)[0], "m070");
        assert!(alice.screen(80, 24).contains("m119"));

        for _ in 0..100 {
            if texts(&alice).len() > PAGE {
                break;
            }
            alice.press(KeyCode::PageUp).await;
        }
        assert_eq!(texts(&alice).len(), 2 * PAGE, "one page more, and the view kept its place");
        assert_eq!(texts(&alice)[0], "m020");
        let expected: Vec<String> = (20..120).map(|i| format!("m{i:03}")).collect();
        assert_eq!(texts(&alice), expected, "in order, each once");
        assert!(alice.state.scroll.is_scrolled());
        alice.close().await;
        bob.close().await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn add_opens_a_tab_by_number() {
        let server = TestServer::start().await;
        let data = tempfile::tempdir().unwrap();
        let mut alice = App::active(&data.path().join("alice"), &server, "alice").await;
        let bob = App::active(&data.path().join("bob"), &server, "bob").await;
        let b = bob.number();

        alice.send("/add 482 910 5574").await;
        assert_eq!(last_corvo(&alice.state).text, "That number has a typo");
        assert_eq!(last_corvo(&alice.state).tone, Tone::Bad);
        assert_eq!(alice.state.tabs.len(), 1);

        alice.send(&format!("/add {}", b.digits())).await;
        assert_eq!(alice.state.tabs.len(), 2);
        assert_eq!(alice.state.open_number(), Some(b));
        assert_eq!(alice.state.tab().title, b.to_string());
        assert!(alice.screen(80, 24).lines().next().unwrap().contains(&format!(" {b} ")));
        assert!(shown(&alice.state, b).is_empty(), "empty until the first message");
        alice.close().await;
        bob.close().await;
    }

    #[test]
    fn status_line_shows_name_number_and_state() {
        let mut state = State::new("Ada".into(), "482 910 5573".into());
        let status = |state: &State| screen_of(state, 80, 24).lines().last().unwrap().trim_end().to_string();
        state.conn = Some(ConnState::Online);
        assert_eq!(status(&state), " Ada · 482 910 5573 · ● online");
        state.conn = Some(ConnState::Reconnecting { in_secs: 4 });
        assert_eq!(status(&state), " Ada · 482 910 5573 · ● reconnecting in 4s");
        state.conn = Some(ConnState::UpdateRequired);
        assert_eq!(status(&state), " Ada · 482 910 5573 · ● update needed");
        let rahul = num(123_456_789);
        state.open_chat(rahul, false);
        state.chats.get_mut(&rahul).unwrap().messages =
            vec![message(1, true, "one", Some(Mark::Waiting)), message(2, true, "two", Some(Mark::Waiting))];
        assert_eq!(status(&state), " Ada · 482 910 5573 · ● update needed · 2 waiting");
        let narrow = screen_of(&state, 40, 12);
        assert!(!narrow.lines().last().unwrap().contains("waiting"), "only as space allows");
    }

    #[test]
    fn a_burst_of_updates_asks_once_then_once_more() {
        let (mut state, _) = ana_and_rahul();
        let mut effects = Vec::new();
        for seq in 1..=5 {
            effects.extend(update(&mut state, Msg::Core(Update::Message { chat_id: [7; 16], seq })));
        }
        let asked = calls(&effects);
        assert_eq!(asked.iter().filter(|c| matches!(c, Command::Chats)).count(), 1, "{asked:?}");
        assert_eq!(asked.iter().filter(|c| matches!(c, Command::Messages { .. })).count(), 1, "{asked:?}");
        let reply = Outcome::Reply { asked: Command::Chats, reply: Ok(Reply::Chats(Vec::new())) };
        let again = update(&mut state, Msg::Done(reply));
        assert_eq!(calls(&again), [&Command::Chats], "once more, for what came meanwhile");
        let reply = Outcome::Reply { asked: Command::Chats, reply: Ok(Reply::Chats(Vec::new())) };
        assert!(calls(&update(&mut state, Msg::Done(reply))).is_empty(), "and then it's done");
    }

    #[test]
    fn the_open_chat_is_marked_read_only_when_seen() {
        let (mut state, rahul) = ana_and_rahul();
        state.chats.get_mut(&rahul).unwrap().messages = vec![message(1, false, "hello", None)];
        state.chats.get_mut(&rahul).unwrap().loaded = true;
        let row = ChatRow {
            chat_id: [7; 16],
            with: rahul,
            name: Some("rahul".into()),
            relation: Relation::Contact,
            last: None,
            unread: 1,
            online: false,
            deleted: false,
        };
        let marks = |effects: &[Effect]| calls(effects).iter().any(|c| matches!(c, Command::MarkRead { .. }));
        update(&mut state, Msg::Focus(false));
        let reply = Outcome::Reply { asked: Command::Chats, reply: Ok(Reply::Chats(vec![row])) };
        assert!(!marks(&update(&mut state, Msg::Done(reply))), "the window isn't focused");
        assert_eq!(state.title, "(1) corvo-chat");
        assert!(marks(&update(&mut state, Msg::Focus(true))), "seen now");
        assert!(!marks(&update(&mut state, Msg::Tick)), "once");
    }

    #[test]
    fn a_failed_send_says_why_and_gives_the_text_back() {
        let (mut state, rahul) = ana_and_rahul();
        for c in "long one".chars() {
            update(&mut state, Msg::Key(KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE)));
        }
        let effects = update(&mut state, Msg::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)));
        let asked = Command::SendText { to: rahul, text: "long one".into() };
        assert_eq!(calls(&effects), [&asked]);
        let failed = Outcome::Reply { asked, reply: Err("That message is too long.".into()) };
        update(&mut state, Msg::Done(failed));
        assert_eq!(state.flash.as_ref().unwrap().line.text, "That message is too long.");
        assert_eq!(state.editor.text(), "long one", "nothing typed is lost");
    }

    #[test]
    fn code_shows_your_number_and_chat_takes_a_name_or_number() {
        let (mut state, rahul) = ana_and_rahul();
        state.select_tab(0);
        let send = |state: &mut State, line: &str| {
            for c in line.chars() {
                update(state, Msg::Key(KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE)));
            }
            update(state, Msg::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)))
        };
        send(&mut state, "/code");
        assert_eq!(last_corvo(&state).text, "Your number is 482 910 5572. Send it to a friend so they can /add you.");
        send(&mut state, "/chat RAHUL");
        assert_eq!(state.open_number(), Some(rahul));
        state.select_tab(0);
        send(&mut state, &format!("/chat {}", rahul.digits()));
        assert_eq!(state.open_number(), Some(rahul));
        assert_eq!(state.tabs.len(), 2, "the same tab");
        send(&mut state, "/chat nobody");
        assert_eq!(last_corvo(&state).text, "No one here is called nobody.");
        send(&mut state, "/chat 123 456 7890");
        assert_eq!(last_corvo(&state).text, TYPO);
        state.people.push(person(num(222_222_222), "Rahul", Relation::Contact));
        send(&mut state, "/chat rahul");
        assert_eq!(last_corvo(&state).text, "More than one person is called rahul. Use their number.");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_call_that_never_answers_leaves_the_screen_live() {
        let server = TestServer::start().await;
        let proxy = Proxy::start(&server).await;
        let data = tempfile::tempdir().unwrap();
        let mut alice = App::active_at(&data.path().join("alice"), &server, proxy.server(&server), "alice").await;
        let bob = App::invited(&data.path().join("bob"), &server, "bob", &mut alice).await;
        let b = bob.number();
        alice.until("bob as a contact", |s| s.person(b).is_some()).await;
        alice.send("/chat bob").await;
        // From here the server hears nothing alice's app says, so never answers.
        proxy.hold(true);
        let key = |c| Msg::Key(KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE));
        let enter = || Msg::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        let live = async {
            for c in "hi".chars() {
                alice.handle_now(key(c)).await.unwrap();
            }
            alice.handle_now(enter()).await.unwrap();
            assert_eq!(alice.calls(), 1, "the send waits for the server");
            for c in format!("/add {}", b.digits()).chars() {
                alice.handle_now(key(c)).await.unwrap();
            }
            alice.handle_now(enter()).await.unwrap();
            assert_eq!(alice.state.busy, Some("Looking them up…"));
            assert!(alice.screen(80, 24).contains("Looking them up…"), "drawn while it waits");
            for c in "still here".chars() {
                alice.handle_now(key(c)).await.unwrap();
            }
            assert_eq!(alice.state.editor.text(), "still here", "keys still work");
            assert!(alice.screen(80, 24).contains("> still here"));
            assert_eq!(alice.calls(), 2, "both still waiting");
            let quit = Msg::Key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL));
            assert_eq!(alice.handle_now(quit).await, Err(None), "Ctrl-C quits");
        };
        tokio::time::timeout(std::time::Duration::from_secs(5), live).await.expect("nothing waits for the server");
        bob.close().await;
    }

    #[test]
    fn busy_text_stays_until_its_own_reply() {
        let (mut state, rahul) = ana_and_rahul();
        state.select_tab(0);
        for c in format!("/add {}", rahul.digits()).chars() {
            update(&mut state, Msg::Key(KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE)));
        }
        update(&mut state, Msg::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)));
        assert_eq!(state.busy, Some("Looking them up…"));
        let chats = Outcome::Reply { asked: Command::Chats, reply: Ok(Reply::Chats(Vec::new())) };
        update(&mut state, Msg::Done(chats));
        assert_eq!(state.busy, Some("Looking them up…"), "another call's reply leaves it");
        let added = Outcome::Reply { asked: Command::Add { number: rahul }, reply: Ok(Reply::Done) };
        update(&mut state, Msg::Done(added));
        assert_eq!(state.busy, None);
    }

    /// Rahul's tab holding seqs `from..=to`, loaded.
    fn loaded(from: i64, to: i64) -> (State, Number) {
        let (mut state, rahul) = ana_and_rahul();
        let chat = state.chats.get_mut(&rahul).unwrap();
        chat.messages = (from..=to).map(|seq| message(seq, false, &format!("m{seq}"), None)).collect();
        chat.loaded = true;
        (state, rahul)
    }

    fn page(from: i64, to: i64) -> Result<Reply, String> {
        Ok(Reply::Messages((from..=to).map(|seq| message(seq, false, &format!("m{seq}"), None)).collect()))
    }

    fn seqs(state: &State, number: Number) -> (i64, i64, usize) {
        let messages = &state.chats[&number].messages;
        (messages[0].seq.unwrap(), messages.last().unwrap().seq.unwrap(), messages.len())
    }

    #[test]
    fn a_reload_asked_before_an_older_page_does_not_cut_it() {
        let (mut state, rahul) = loaded(71, 120);
        let effects = update(&mut state, Msg::Core(Update::Message { chat_id: [7; 16], seq: 121 }));
        let reload = Command::Messages { chat_id: [7; 16], before: None, limit: 50 + PAGE };
        assert!(calls(&effects).contains(&&reload), "{effects:?}");
        // The older page comes first, then the reload asked before it.
        let older_page = Command::Messages { chat_id: [7; 16], before: Some(71), limit: PAGE };
        update(&mut state, Msg::Done(Outcome::Reply { asked: older_page, reply: page(21, 70) }));
        assert_eq!(seqs(&state, rahul), (21, 120, 100));
        let effects = update(&mut state, Msg::Done(Outcome::Reply { asked: reload, reply: page(22, 121) }));
        assert_eq!(seqs(&state, rahul), (21, 120, 100), "the page stays whole");
        let again = Command::Messages { chat_id: [7; 16], before: None, limit: 100 + PAGE };
        assert!(calls(&effects).contains(&&again), "asked again, reaching the page: {effects:?}");
        update(&mut state, Msg::Done(Outcome::Reply { asked: again, reply: page(1, 121) }));
        assert_eq!(seqs(&state, rahul), (21, 121, 101), "the new one added, nothing older pulled in");
    }

    #[test]
    fn a_complete_history_stops_at_the_last_saved_page() {
        for (complete, more) in [(true, false), (false, true)] {
            let (mut state, rahul) = loaded(71, 120);
            state.chats.get_mut(&rahul).unwrap().loading = true;
            let older =
                Outcome::Reply { asked: Command::Older { chat_id: [7; 16] }, reply: Ok(Reply::Complete(complete)) };
            let effects = update(&mut state, Msg::Done(older));
            let cached = Command::Messages { chat_id: [7; 16], before: Some(71), limit: PAGE };
            assert_eq!(calls(&effects), [&cached]);
            update(&mut state, Msg::Done(Outcome::Reply { asked: cached, reply: page(41, 70) }));
            assert_eq!(seqs(&state, rahul), (41, 120, 80));
            assert_eq!(state.chats[&rahul].more, more, "complete: {complete}");
        }
    }
}
