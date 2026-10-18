//! Every change to the screen's state, one message at a time. It never calls
//! the core and draws nothing: it returns [`Effect`]s for the loop to carry
//! out, and their results come back as messages.

use corvo_chat_core::Update;
use ratatui::crossterm::event::{
    KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
};
use ratatui::layout::Position;

use super::commands::{Cmd, Input, parse, usage};
use super::complete;
use super::state::{APP, Screen, State, Tone, WHEEL_STEP};
use super::view::{areas, chat_page, input_rows, popup_area, tab_at};
use super::window::{self, Overlay};

#[derive(Debug)]
pub enum Msg {
    Key(KeyEvent),
    Paste(String),
    Mouse(MouseEvent),
    Resize(u16, u16),
    /// Four times a second, so flashes go away on time.
    Tick,
    /// From the core, once it runs.
    Core(Update),
    /// What came of an [`Effect`].
    #[allow(dead_code, reason = "the effects that sign in and chat have outcomes")]
    Done(Outcome),
    /// The window gained or lost focus.
    #[allow(dead_code, reason = "chats are marked read only while it has focus")]
    Focus(bool),
}

/// Work for the loop: anything that touches the core, files or the
/// terminal. Its result comes back as [`Msg::Done`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Effect {
    Quit,
    /// Leave, and print this once the terminal is back to normal.
    #[allow(dead_code, reason = "a failure that leaves the app unusable will use it")]
    Exit(String),
    SetTitle(String),
}

impl Effect {
    /// What to show while it runs, for the ones that take a moment.
    pub fn busy(&self) -> Option<&'static str> {
        match self {
            Effect::Quit | Effect::Exit(_) | Effect::SetTitle(_) => None,
        }
    }
}

/// What came of an [`Effect`]. None of the effects so far has one.
#[derive(Debug)]
pub enum Outcome {}

/// What a key press means.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Action {
    Send,
    Newline,
    Insert(char),
    Backspace,
    Delete,
    Left,
    Right,
    Home,
    End,
    Up,
    Down,
    NextTab,
    PrevTab,
    GoTab(usize),
    PageUp,
    PageDown,
    Esc,
    KillRow,
    KillWord,
    Clear,
    Quit,
    Complete,
}

impl Action {
    /// Changes the text being typed.
    fn edits(self) -> bool {
        matches!(self, Action::Insert(_) | Action::Backspace | Action::Delete | Action::KillRow | Action::KillWord)
    }
}

/// Shift+Enter needs a terminal that reports it; Alt+Enter and Ctrl-J work everywhere.
fn action_for(key: KeyEvent) -> Option<Action> {
    if key.kind == KeyEventKind::Release {
        return None;
    }
    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
    let alt = key.modifiers.contains(KeyModifiers::ALT);
    let shift = key.modifiers.contains(KeyModifiers::SHIFT);
    Some(match key.code {
        KeyCode::Enter if shift || alt => Action::Newline,
        KeyCode::Enter => Action::Send,
        KeyCode::Char('c') if ctrl => Action::Quit,
        KeyCode::Char('l') if ctrl => Action::Clear,
        KeyCode::Char('u') if ctrl => Action::KillRow,
        KeyCode::Char('w') if ctrl => Action::KillWord,
        KeyCode::Char('j') if ctrl => Action::Newline,
        KeyCode::Char(c @ '1'..='9') if alt => Action::GoTab(c as usize - '1' as usize),
        // Windows reports AltGr chords, like @ on a German keyboard, as Ctrl+Alt.
        KeyCode::Char(c) if ctrl && alt => Action::Insert(c),
        KeyCode::Char(_) if ctrl || alt => return None,
        KeyCode::Char(c) => Action::Insert(c),
        KeyCode::Backspace => Action::Backspace,
        KeyCode::Delete => Action::Delete,
        KeyCode::Left if alt => Action::PrevTab,
        KeyCode::Right if alt => Action::NextTab,
        KeyCode::Left => Action::Left,
        KeyCode::Right => Action::Right,
        KeyCode::Home => Action::Home,
        KeyCode::End => Action::End,
        KeyCode::Up => Action::Up,
        KeyCode::Down => Action::Down,
        KeyCode::BackTab => Action::NextTab,
        KeyCode::Tab => Action::Complete,
        KeyCode::PageUp => Action::PageUp,
        KeyCode::PageDown => Action::PageDown,
        KeyCode::Esc => Action::Esc,
        _ => return None,
    })
}

pub fn update(state: &mut State, msg: Msg) -> Vec<Effect> {
    let mut effects = Vec::new();
    if !matches!(msg, Msg::Tick) {
        state.changes += 1;
    }
    match msg {
        // A window takes every key but Ctrl-C.
        Msg::Key(key) if state.overlay.is_some() => match action_for(key) {
            Some(Action::Quit) => on_action(state, Action::Quit, &mut effects),
            _ if key.kind != KeyEventKind::Release => window::key(state, key),
            _ => {}
        },
        Msg::Mouse(mouse) if state.overlay.is_some() => match mouse.kind {
            MouseEventKind::ScrollUp => window::scroll_by(state, -(WHEEL_STEP as isize)),
            MouseEventKind::ScrollDown => window::scroll_by(state, WHEEL_STEP as isize),
            _ => {}
        },
        Msg::Paste(_) if state.overlay.is_some() => {}
        Msg::Key(key) => {
            if let Some(action) = action_for(key) {
                on_action(state, action, &mut effects);
            }
        }
        Msg::Paste(text) => {
            state.popup = None;
            state.editor.insert_str(&text);
        }
        Msg::Mouse(mouse) => match mouse.kind {
            MouseEventKind::ScrollUp => scroll_by(state, -(WHEEL_STEP as isize)),
            MouseEventKind::ScrollDown => scroll_by(state, WHEEL_STEP as isize),
            MouseEventKind::Down(MouseButton::Left) => click(state, mouse.column, mouse.row),
            _ => {}
        },
        Msg::Resize(w, h) => {
            state.size = (w, h);
            drop_hidden_popup(state);
        }
        // Nothing needs it yet: chats will only be marked read while it's focused.
        Msg::Focus(_) => {}
        Msg::Tick => {
            window::tick(state);
            if let Some(flash) = &mut state.flash {
                flash.ticks_left = flash.ticks_left.saturating_sub(1);
                if flash.ticks_left == 0 {
                    state.flash = None;
                }
            }
        }
        Msg::Core(update) => on_core(state, update),
        Msg::Done(outcome) => {
            state.busy = None;
            match outcome {}
        }
    }
    if let Some(busy) = effects.iter().find_map(Effect::busy) {
        state.busy = Some(busy);
    }
    if state.title != APP {
        state.title = APP.into();
        effects.push(Effect::SetTitle(APP.into()));
    }
    effects
}

/// A left click: on a tab selects it; one on the completion list takes that choice.
fn click(state: &mut State, x: u16, y: u16) {
    let Some(areas) = areas(state.size, input_rows(state)) else { return };
    if !matches!(state.screen, Screen::Main) {
        return;
    }
    // The completion list sits over the chat: a click on it is for it.
    if let Some((list, first, rows)) = popup_area(state, areas.input)
        && list.contains(Position::new(x, y))
    {
        if (list.y + 1..list.y + 1 + rows as u16).contains(&y)
            && let Some(popup) = &mut state.popup
        {
            popup.selected = first + usize::from(y - list.y - 1);
            complete::accept(state);
        }
        return;
    }
    if y == areas.tabs.y
        && let Some(tab) = tab_at(state, x)
    {
        state.select_tab(tab);
    }
}

fn scroll_by(state: &mut State, delta: isize) {
    let (total, height) = chat_page(state);
    state.scroll.by(delta, total, height);
}

/// A completion list that isn't drawn closes, so keys don't go to it unseen.
fn drop_hidden_popup(state: &mut State) {
    let drawn = areas(state.size, input_rows(state)).is_some_and(|a| popup_area(state, a.input).is_some());
    if !drawn {
        state.popup = None;
    }
}

fn on_action(state: &mut State, action: Action, effects: &mut Vec<Effect>) {
    drop_hidden_popup(state);
    if state.popup.is_some() {
        match action {
            Action::Complete | Action::Down => return complete::step(state, 1),
            Action::Up => return complete::step(state, -1),
            Action::Send => return complete::accept(state),
            Action::Esc => {
                state.popup = None;
                return;
            }
            a if a.edits() => {}
            _ => state.popup = None,
        }
    }
    on_key(state, action, effects);
    if action.edits() {
        complete::refresh(state);
    }
}

fn on_key(state: &mut State, action: Action, effects: &mut Vec<Effect>) {
    let editor = &mut state.editor;
    match action {
        Action::Send => {
            let line = editor.take();
            state.history.push(&line);
            on_line(state, &line, effects);
        }
        Action::Complete => complete::tab(state),
        Action::Quit => effects.push(Effect::Quit),
        Action::Clear => state.clear_tab(),
        Action::Newline => editor.newline(),
        Action::Insert(c) => editor.insert(c),
        Action::Backspace => editor.backspace(),
        Action::Delete => editor.delete(),
        Action::Left => editor.left(),
        Action::Right => editor.right(),
        Action::Home => editor.home(),
        Action::End => editor.end(),
        Action::KillRow => editor.kill_to_row_start(),
        Action::KillWord => editor.kill_word(),
        Action::Up => {
            if !editor.up()
                && let Some(entry) = state.history.prev(editor.text())
            {
                editor.set(entry);
            }
        }
        Action::Down => {
            if !editor.down()
                && let Some(entry) = state.history.next()
            {
                editor.set(entry);
            }
        }
        Action::NextTab => state.step_tab(1),
        Action::PrevTab => state.step_tab(-1),
        Action::GoTab(i) => state.select_tab(i),
        Action::PageUp => {
            let (total, height) = chat_page(state);
            state.scroll.page_up(total, height);
        }
        Action::PageDown => {
            let (total, height) = chat_page(state);
            state.scroll.page_down(total, height);
        }
        Action::Esc if editor.is_empty() => state.scroll.follow_end(),
        Action::Esc => editor.clear(),
    }
}

/// A line sent with Enter.
fn on_line(state: &mut State, line: &str, effects: &mut Vec<Effect>) {
    match parse(line) {
        Input::Empty => {}
        Input::Text(_) => state.say(Tone::Bad, "this is the corvo tab: switch to a chat (Shift-Tab) to send messages"),
        Input::Wrong(why) => state.say(Tone::Bad, why),
        Input::Usage(name) => usage_card(state, name),
        Input::Cmd(cmd) => on_command(state, cmd, effects),
    }
}

/// How the command called `name` is typed, what it does and an example, in
/// the corvo tab.
fn usage_card(state: &mut State, name: &str) {
    let Some(card) = usage(name) else { return };
    state.select_tab(0);
    state.say(Tone::Plain, card.usage);
    state.say(Tone::Dim, card.what);
    if let Some(example) = card.example {
        state.say(Tone::Dim, format!("e.g.  {example}"));
    }
}

fn on_command(state: &mut State, cmd: Cmd, effects: &mut Vec<Effect>) {
    match cmd {
        Cmd::Help => state.overlay = Some(Overlay::Help { scroll: 0 }),
        Cmd::Clear => state.clear_tab(),
        Cmd::About => {
            let data_dir = state.data_dir.clone().unwrap_or_else(|| "(unknown)".into());
            state.overlay = Some(Overlay::About { data_dir, scroll: 0 });
        }
        Cmd::Quit => effects.push(Effect::Quit),
        Cmd::Close => {
            if !state.close_tab() {
                state.say(Tone::Bad, "the corvo tab stays open; /quit leaves the app");
            }
        }
        other => state.say(Tone::Plain, format!("Not yet: {}", other.typed())),
    }
}

/// Only where the connection is, for now.
fn on_core(state: &mut State, update: Update) {
    if let Update::Connection(conn) = update {
        state.conn = Some(conn);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ui::state::{FLASH_TICKS, Line};
    use corvo_chat_core::ConnState;
    use ratatui::crossterm::event::KeyEventState;

    fn state() -> State {
        State::new("ana".into(), "482 910 5573".into())
    }

    fn key(code: KeyCode, modifiers: KeyModifiers) -> Msg {
        Msg::Key(KeyEvent::new(code, modifiers))
    }

    fn press(state: &mut State, code: KeyCode) -> Vec<Effect> {
        update(state, key(code, KeyModifiers::NONE))
    }

    fn alt(state: &mut State, code: KeyCode) -> Vec<Effect> {
        update(state, key(code, KeyModifiers::ALT))
    }

    fn ctrl(state: &mut State, c: char) -> Vec<Effect> {
        update(state, key(KeyCode::Char(c), KeyModifiers::CONTROL))
    }

    fn type_text(state: &mut State, text: &str) {
        for c in text.chars() {
            press(state, KeyCode::Char(c));
        }
    }

    fn send(state: &mut State, text: &str) -> Vec<Effect> {
        type_text(state, text);
        press(state, KeyCode::Enter)
    }

    fn last_corvo(state: &State) -> &Line {
        state.tabs[0].lines.last().expect("the corvo tab has lines")
    }

    #[test]
    fn a_usage_card_prints_three_lines_in_the_corvo_tab() {
        let mut s = state();
        let from = s.tabs[0].lines.len();
        assert!(send(&mut s, "/chat").is_empty());
        let card: Vec<(&str, Tone)> = s.tabs[0].lines[from..].iter().map(|l| (l.text.as_str(), l.tone)).collect();
        assert_eq!(
            card,
            [
                ("/chat <name|number>", Tone::Plain),
                ("Opens a chat with a contact or a request.", Tone::Dim),
                ("e.g.  /chat rahul", Tone::Dim),
            ]
        );
        assert_eq!(s.flash, None, "a card isn't an error");
        let from = s.tabs[0].lines.len();
        send(&mut s, "/clear now");
        assert_eq!(s.tabs[0].lines.len() - from, 2, "no example for a command that takes nothing");
    }

    #[test]
    fn commands_of_later_versions_say_not_yet() {
        let mut s = state();
        for line in ["/code", "/add 482 910 5573", "/invite", "/contacts", "/profile", "/verify rahul", "/reject"] {
            assert!(send(&mut s, line).is_empty(), "{line}");
            let said = last_corvo(&s);
            assert!(said.text.starts_with("Not yet: /"), "{line}: {said:?}");
            assert_eq!(said.tone, Tone::Plain, "{line}");
        }
        assert_eq!(s.flash, None, "not an error");
        assert_eq!(s.overlay, None);
    }

    #[test]
    fn typing_editing_and_alt_enter() {
        let mut s = state();
        type_text(&mut s, "helo");
        press(&mut s, KeyCode::Left);
        type_text(&mut s, "l");
        press(&mut s, KeyCode::End);
        alt(&mut s, KeyCode::Enter);
        type_text(&mut s, "world");
        assert_eq!(s.editor.text(), "hello\nworld");
        update(&mut s, key(KeyCode::Enter, KeyModifiers::SHIFT));
        ctrl(&mut s, 'j');
        assert_eq!(s.editor.text(), "hello\nworld\n\n");
        press(&mut s, KeyCode::Backspace);
        press(&mut s, KeyCode::Backspace);
        ctrl(&mut s, 'w');
        assert_eq!(s.editor.text(), "hello\n");
        press(&mut s, KeyCode::Esc);
        assert!(s.editor.is_empty(), "Esc clears the input");
    }

    #[test]
    fn key_releases_and_unknown_shortcuts_do_nothing() {
        let mut s = state();
        let mut release = KeyEvent::new(KeyCode::Char('x'), KeyModifiers::NONE);
        release.kind = KeyEventKind::Release;
        release.state = KeyEventState::NONE;
        update(&mut s, Msg::Key(release));
        ctrl(&mut s, 'x');
        alt(&mut s, KeyCode::Char('x'));
        assert!(s.editor.is_empty());
    }

    #[test]
    fn up_and_down_browse_sent_lines_after_the_input_rows() {
        let mut s = state();
        send(&mut s, "/blocked");
        send(&mut s, "/clear");
        type_text(&mut s, "draft");
        press(&mut s, KeyCode::Up);
        assert_eq!(s.editor.text(), "/clear");
        press(&mut s, KeyCode::Up);
        assert_eq!(s.editor.text(), "/blocked");
        press(&mut s, KeyCode::Down);
        press(&mut s, KeyCode::Down);
        assert_eq!(s.editor.text(), "draft");

        s.editor.set("one\ntwo".into());
        press(&mut s, KeyCode::Up);
        assert_eq!(s.editor.text(), "one\ntwo", "first ↑ moves within the input");
        assert_eq!(s.editor.cursor_pos().0, 0);
    }

    #[test]
    fn every_tab_key_stays_on_the_only_tab() {
        let mut s = state();
        update(&mut s, key(KeyCode::BackTab, KeyModifiers::SHIFT));
        alt(&mut s, KeyCode::Left);
        alt(&mut s, KeyCode::Right);
        alt(&mut s, KeyCode::Char('2'));
        assert_eq!(s.current, 0, "no second tab: nothing changes");
        alt(&mut s, KeyCode::Char('1'));
        assert!(s.on_corvo());
    }

    #[test]
    fn page_keys_and_the_wheel_scroll() {
        let mut s = state();
        for i in 0..100 {
            s.say(Tone::Plain, format!("line {i}"));
        }
        let (total, height) = chat_page(&s);
        press(&mut s, KeyCode::PageUp);
        assert_eq!(s.scroll.top(total, height), total - 2 * height);
        press(&mut s, KeyCode::PageDown);
        assert!(!s.scroll.is_scrolled());
        let wheel = |kind| Msg::Mouse(MouseEvent { kind, column: 30, row: 5, modifiers: KeyModifiers::NONE });
        update(&mut s, wheel(MouseEventKind::ScrollUp));
        assert_eq!(s.scroll.top(total, height), total - height - WHEEL_STEP);
        update(&mut s, wheel(MouseEventKind::ScrollDown));
        assert!(!s.scroll.is_scrolled());
        update(&mut s, wheel(MouseEventKind::ScrollUp));
        press(&mut s, KeyCode::Esc);
        assert!(!s.scroll.is_scrolled(), "Esc on an empty input goes back to the bottom");
    }

    #[test]
    fn switching_tabs_returns_to_the_newest_lines() {
        let mut s = state();
        for i in 0..100 {
            s.say(Tone::Plain, format!("line {i}"));
        }
        press(&mut s, KeyCode::PageUp);
        assert!(s.scroll.is_scrolled());
        alt(&mut s, KeyCode::Char('1'));
        assert!(!s.scroll.is_scrolled());
    }

    #[test]
    fn clear_and_ctrl_l_empty_the_open_tab() {
        let mut s = state();
        s.say(Tone::Plain, "old");
        send(&mut s, "/clear");
        assert_eq!(s.tabs[0].cleared, 1);
        s.say(Tone::Plain, "new");
        ctrl(&mut s, 'l');
        assert_eq!(s.tabs[0].cleared, 2);
        assert_eq!(s.tabs[0].lines.len(), 2, "nothing is deleted");
    }

    #[test]
    fn quit_and_ctrl_c() {
        let mut s = state();
        assert!(send(&mut s, "/quit").contains(&Effect::Quit));
        assert!(send(&mut s, "/q").contains(&Effect::Quit));
        assert!(ctrl(&mut s, 'c').contains(&Effect::Quit));
        assert!(!send(&mut s, "/help").contains(&Effect::Quit));
    }

    #[test]
    fn pasting_keeps_line_breaks_as_newlines() {
        let mut s = state();
        type_text(&mut s, "> ");
        update(&mut s, Msg::Paste("one\r\ntwo\rthree\n".into()));
        assert_eq!(s.editor.text(), "> one\ntwo\nthree\n");
        type_text(&mut s, "!");
        assert!(s.editor.text().ends_with("\n!"), "the cursor is after the paste");
    }

    #[test]
    fn close_does_not_close_corvo() {
        let mut s = state();
        send(&mut s, "/close");
        assert_eq!(s.tabs.len(), 1);
        assert_eq!(last_corvo(&s).tone, Tone::Bad);
        assert!(last_corvo(&s).text.contains("corvo tab stays open"));
    }

    #[test]
    fn mistakes_are_said_and_flashed() {
        let mut s = state();
        send(&mut s, "/acept");
        assert_eq!(last_corvo(&s).text, "unknown command /acept: did you mean /accept?");
        assert_eq!(s.flash.as_ref().unwrap().line, *last_corvo(&s));
        send(&mut s, "hello");
        assert!(last_corvo(&s).text.starts_with("this is the corvo tab"));
        let before = s.tabs[0].lines.len();
        send(&mut s, "   ");
        assert_eq!(s.tabs[0].lines.len(), before, "an empty line does nothing");
    }

    #[test]
    fn a_flash_goes_away_after_its_ticks() {
        let mut s = state();
        send(&mut s, "/nope");
        for _ in 1..FLASH_TICKS {
            update(&mut s, Msg::Tick);
        }
        assert!(s.flash.is_some());
        update(&mut s, Msg::Tick);
        assert!(s.flash.is_none());
    }

    #[test]
    fn the_title_is_set_once() {
        let mut s = state();
        assert_eq!(update(&mut s, Msg::Tick), vec![Effect::SetTitle(APP.into())]);
        assert_eq!(update(&mut s, Msg::Tick), vec![]);
    }

    #[test]
    fn the_connection_state_comes_from_the_core() {
        let mut s = state();
        assert_eq!(s.conn, None, "connecting until it says");
        update(&mut s, Msg::Core(Update::Connection(ConnState::Online)));
        assert_eq!(s.conn, Some(ConnState::Online));
        update(&mut s, Msg::Core(Update::Connection(ConnState::Reconnecting { in_secs: 4 })));
        assert_eq!(s.conn, Some(ConnState::Reconnecting { in_secs: 4 }));
        update(&mut s, Msg::Core(Update::Synced));
        assert_eq!(s.conn, Some(ConnState::Reconnecting { in_secs: 4 }), "other updates leave it");
    }

    #[test]
    fn a_list_that_is_not_drawn_takes_no_keys() {
        let mut s = state();
        send(&mut s, "/blocked");
        type_text(&mut s, "/c");
        press(&mut s, KeyCode::Tab);
        assert!(s.popup.is_some());
        update(&mut s, Msg::Resize(80, 8));
        assert_eq!(s.popup, None, "a resize to too small clears it");

        update(&mut s, Msg::Resize(80, 24));
        press(&mut s, KeyCode::Tab);
        assert!(s.popup.is_some());
        // Drawn nowhere, as if the window shrank without saying.
        s.size = (80, 8);
        press(&mut s, KeyCode::Up);
        assert_eq!(s.popup, None);
        assert_eq!(s.editor.text(), "/blocked", "↑ browses the history as if there were no list");
    }

    #[test]
    fn resize_changes_the_page_size() {
        let mut s = state();
        update(&mut s, Msg::Resize(100, 40));
        assert_eq!(s.size, (100, 40));
        assert_eq!(chat_page(&s).1, 40 - 5);
    }
}
