//! What's on screen: the tabs and their lines, the input box and its history,
//! scrolling, the flash and the status line. Only `update` changes it; `view`
//! only reads it.

use std::collections::{HashMap, HashSet};
use std::fmt;
use std::sync::{Arc, Mutex};

use corvo_chat_core::native::Command;
use corvo_chat_core::{ChatRow, ConnState, Mark, MsgId, PersonRow, ShownMessage};
use corvo_chat_proto::Number;
use zeroize::Zeroizing;

use super::chat::Loads;
use super::people::Pending;
use super::start::Start;
use super::view::clean;
use super::window::Overlay;

pub const APP: &str = "corvo-chat";
pub const VERSION: &str = env!("CARGO_PKG_VERSION");
/// The input box grows up to this many rows, then scrolls.
pub const MAX_INPUT_ROWS: usize = 5;
pub const HISTORY_LEN: usize = 100;
/// Lines kept in the `corvo` tab; older ones are dropped.
pub const CORVO_LEN: usize = 500;
/// How long a flash stays, in ticks (5 seconds).
pub const FLASH_TICKS: u32 = 20;
/// Rows the mouse wheel scrolls per notch.
pub const WHEEL_STEP: usize = 3;

/// Text that mustn't linger: a password, a recovery code or an invite code.
/// Wiped from memory when dropped, and `{:?}` never shows it.
#[derive(Clone, Default, PartialEq, Eq)]
pub struct Secret(Zeroizing<String>);

impl Secret {
    pub fn new(text: impl Into<Zeroizing<String>>) -> Secret {
        Secret(text.into())
    }

    pub fn expose(&self) -> &str {
        &self.0
    }

    /// The text, moved (not copied) into what the core takes.
    pub fn into_zeroizing(self) -> Zeroizing<String> {
        self.0
    }
}

impl fmt::Debug for Secret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Secret(..)")
    }
}

/// A question in the input box. The answer goes to whoever asked, not to a
/// chat, and isn't kept in the input history.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Ask {
    /// Shown before the answer, in place of `> `.
    pub label: String,
    /// Each character shows as `•`.
    pub masked: bool,
}

/// Completion choices shown above the input.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Popup {
    pub matches: Vec<String>,
    pub selected: usize,
    /// Byte offset in the input where the word being completed starts.
    pub start: usize,
}

/// Which screen is up.
pub enum Screen {
    /// Signing up, in or back in, or unlocking, before the chats. Boxed:
    /// it's far bigger than the main screen's nothing.
    Start(Box<Start>),
    /// The tabs, the input and the status line.
    Main,
}

/// Room the input box makes before an answer is typed, so it never moves
/// while a password is in it (a move would leave a copy behind).
pub const ANSWER_ROOM: usize = 1024;

/// The multi-line input box. `cursor` is a byte index on a char boundary.
/// What's typed is wiped from memory when it's replaced or dropped.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Editor {
    text: Zeroizing<String>,
    cursor: usize,
}

impl Editor {
    pub fn text(&self) -> &str {
        &self.text
    }

    pub fn is_empty(&self) -> bool {
        self.text.is_empty()
    }

    /// Bytes the buffer holds before it has to move.
    #[cfg(test)]
    pub fn capacity(&self) -> usize {
        self.text.capacity()
    }

    pub fn set(&mut self, text: String) {
        self.cursor = text.len();
        self.text = Zeroizing::new(text);
    }

    /// Makes [`ANSWER_ROOM`] before an answer is typed. The text moves to the
    /// new buffer and the old one is wiped.
    pub fn make_room_for_answer(&mut self) {
        if self.text.capacity() < ANSWER_ROOM {
            let mut roomy = Zeroizing::new(String::with_capacity(ANSWER_ROOM.max(self.text.len())));
            roomy.push_str(&self.text);
            self.text = roomy;
        }
    }

    pub fn clear(&mut self) {
        self.set(String::new());
    }

    /// The text, moved out (not copied); the box is left empty.
    pub fn take(&mut self) -> String {
        self.cursor = 0;
        std::mem::take(&mut *self.text)
    }

    pub fn before_cursor(&self) -> &str {
        &self.text[..self.cursor]
    }

    /// Replaces the text from `start` (a byte offset) to the cursor.
    pub fn replace_before_cursor(&mut self, start: usize, with: &str) {
        self.text.replace_range(start..self.cursor, with);
        self.cursor = start + with.len();
    }

    pub fn insert(&mut self, c: char) {
        self.text.insert(self.cursor, c);
        self.cursor += c.len_utf8();
    }

    /// Pasted text: CRLF and lone CR become newlines, tabs become spaces.
    pub fn insert_str(&mut self, s: &str) {
        let mut pasted = Zeroizing::new(String::with_capacity(s.len() * 4));
        let mut chars = s.chars().peekable();
        while let Some(c) = chars.next() {
            match c {
                '\r' => {
                    chars.next_if_eq(&'\n');
                    pasted.push('\n');
                }
                '\t' => pasted.push_str("    "),
                c => pasted.push(c),
            }
        }
        self.text.insert_str(self.cursor, &pasted);
        self.cursor += pasted.len();
    }

    pub fn newline(&mut self) {
        self.insert('\n');
    }

    pub fn backspace(&mut self) {
        if let Some(c) = self.text[..self.cursor].chars().next_back() {
            self.cursor -= c.len_utf8();
            self.text.remove(self.cursor);
        }
    }

    pub fn delete(&mut self) {
        if self.cursor < self.text.len() {
            self.text.remove(self.cursor);
        }
    }

    pub fn left(&mut self) {
        if let Some(c) = self.text[..self.cursor].chars().next_back() {
            self.cursor -= c.len_utf8();
        }
    }

    pub fn right(&mut self) {
        if let Some(c) = self.text[self.cursor..].chars().next() {
            self.cursor += c.len_utf8();
        }
    }

    fn row_start(&self) -> usize {
        self.text[..self.cursor].rfind('\n').map_or(0, |i| i + 1)
    }

    fn row_end(&self) -> usize {
        self.text[self.cursor..].find('\n').map_or(self.text.len(), |i| self.cursor + i)
    }

    pub fn home(&mut self) {
        self.cursor = self.row_start();
    }

    pub fn end(&mut self) {
        self.cursor = self.row_end();
    }

    /// Ctrl-U: delete from the start of the row to the cursor.
    pub fn kill_to_row_start(&mut self) {
        let start = self.row_start();
        self.text.replace_range(start..self.cursor, "");
        self.cursor = start;
    }

    /// Ctrl-W: delete the word before the cursor, and the spaces after it.
    pub fn kill_word(&mut self) {
        let start = self.row_start();
        let trimmed = self.text[start..self.cursor].trim_end_matches(' ');
        let from = start + trimmed.rfind(' ').map_or(0, |i| i + 1);
        self.text.replace_range(from..self.cursor, "");
        self.cursor = from;
    }

    pub fn rows(&self) -> Vec<&str> {
        self.text.split('\n').collect()
    }

    /// (row, column in chars).
    pub fn cursor_pos(&self) -> (usize, usize) {
        let row = self.text[..self.cursor].matches('\n').count();
        (row, self.text[self.row_start()..self.cursor].chars().count())
    }

    /// Byte offset of `col` chars into row `row`, clamped to its end.
    fn offset(&self, row: usize, col: usize) -> usize {
        let start: usize = self.text.split('\n').take(row).map(|r| r.len() + 1).sum();
        let line = self.text.split('\n').nth(row).unwrap_or("");
        start + line.char_indices().nth(col).map_or(line.len(), |(i, _)| i)
    }

    /// Moves to the same column one row up; false on the first row.
    pub fn up(&mut self) -> bool {
        let (row, col) = self.cursor_pos();
        if row == 0 {
            return false;
        }
        self.cursor = self.offset(row - 1, col);
        true
    }

    /// Moves to the same column one row down; false on the last row.
    pub fn down(&mut self) -> bool {
        let (row, col) = self.cursor_pos();
        if row + 1 >= self.rows().len() {
            return false;
        }
        self.cursor = self.offset(row + 1, col);
        true
    }
}

/// Sent lines, browsed with ↑/↓ from the input's first or last row.
#[derive(Debug, Default)]
pub struct History {
    entries: Vec<String>,
    at: Option<usize>,
    /// What was being typed when browsing started.
    draft: String,
}

impl History {
    pub fn push(&mut self, entry: &str) {
        self.at = None;
        if entry.trim().is_empty() || self.entries.last().is_some_and(|last| last == entry) {
            return;
        }
        self.entries.push(entry.to_string());
        if self.entries.len() > HISTORY_LEN {
            self.entries.remove(0);
        }
    }

    pub fn prev(&mut self, current: &str) -> Option<String> {
        let at = match self.at {
            None if self.entries.is_empty() => return None,
            None => {
                self.draft = current.to_string();
                self.entries.len() - 1
            }
            Some(0) => return None,
            Some(i) => i - 1,
        };
        self.at = Some(at);
        Some(self.entries[at].clone())
    }

    /// The next newer entry, then the draft, then nothing.
    #[allow(clippy::should_implement_trait)]
    pub fn next(&mut self) -> Option<String> {
        let at = self.at?;
        if at + 1 < self.entries.len() {
            self.at = Some(at + 1);
            Some(self.entries[at + 1].clone())
        } else {
            self.at = None;
            Some(std::mem::take(&mut self.draft))
        }
    }
}

/// Where the chat area is scrolled. `top` is the first visible row counted
/// from the top, so new rows at the bottom don't move a scrolled view; `None`
/// follows the end.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Scroll {
    top: Option<usize>,
}

impl Scroll {
    pub fn top(&self, total: usize, height: usize) -> usize {
        let max = total.saturating_sub(height);
        self.top.map_or(max, |t| t.min(max))
    }

    pub fn is_scrolled(&self) -> bool {
        self.top.is_some()
    }

    pub fn by(&mut self, delta: isize, total: usize, height: usize) {
        let max = total.saturating_sub(height);
        let next = (self.top(total, height) as isize + delta).clamp(0, max as isize) as usize;
        if next >= max { self.follow_end() } else { self.top = Some(next) }
    }

    pub fn page_up(&mut self, total: usize, height: usize) {
        self.by(-(height.max(1) as isize), total, height);
    }

    pub fn page_down(&mut self, total: usize, height: usize) {
        self.by(height.max(1) as isize, total, height);
    }

    pub fn follow_end(&mut self) {
        self.top = None;
    }

    /// Keeps the same rows in view after `rows` more are added above them.
    pub fn shift(&mut self, rows: usize) {
        if let Some(top) = &mut self.top {
            *top += rows;
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tone {
    Plain,
    /// Less important, like a usage card's example.
    Dim,
    /// Something that worked.
    Good,
    Warn,
    Bad,
    /// Your own messages.
    Own,
}

/// One line of a tab. It may hold line breaks; `view` splits and wraps it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Line {
    pub text: String,
    pub tone: Tone,
}

/// What a tab shows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TabKind {
    /// The app's own lines.
    Corvo,
    /// A chat, and who it's with.
    Chat(Number),
    /// Your account: its menu (see `profile`).
    Profile,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Tab {
    pub kind: TabKind,
    pub title: String,
    /// The `corvo` tab's lines. A chat tab draws its messages from `State::chats`.
    pub lines: Vec<Line>,
    /// How many lines came before the last `/clear`. They're kept, just
    /// scrolled out of view. A chat keeps its own mark, in [`Chat::cleared`].
    pub cleared: usize,
}

impl Tab {
    pub fn corvo() -> Tab {
        Tab { kind: TabKind::Corvo, title: "corvo".into(), lines: Vec::new(), cleared: 0 }
    }

    pub fn chat(number: Number, title: String) -> Tab {
        Tab { kind: TabKind::Chat(number), title, lines: Vec::new(), cleared: 0 }
    }

    pub fn profile() -> Tab {
        Tab { kind: TabKind::Profile, title: "profile".into(), lines: Vec::new(), cleared: 0 }
    }

    /// Who the chat is with, if it's a chat.
    pub fn number(&self) -> Option<Number> {
        match self.kind {
            TabKind::Chat(number) => Some(number),
            TabKind::Corvo | TabKind::Profile => None,
        }
    }
}

/// An open chat tab's messages, oldest first, as last loaded from the core.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Chat {
    /// `None` while there's no chat yet: after `/add`, until the first message.
    pub chat_id: Option<[u8; 16]>,
    pub messages: Vec<ShownMessage>,
    /// Loaded once already: reloads then keep what's shown from its oldest on.
    pub loaded: bool,
    /// There may be older messages than these.
    pub more: bool,
    /// Older ones have been asked for.
    pub loading: bool,
    /// The server said it has nothing older than what's saved here.
    pub complete: bool,
    /// Older pages put in front so far.
    pub pages: u64,
    /// `pages` when the newest messages were last asked for: a reply asked
    /// before an older page came would cut that page off again.
    pub asked_at: u64,
    /// The newest of their messages this tab has marked read.
    pub marked: Option<i64>,
    /// The newest message when the tab was last cleared.
    pub cleared: Option<MsgId>,
    /// Messages typed here that the core refused outright, so never saved
    /// (their keys changed): shown after the rest, `✗`, until the tab closes.
    pub unsent: Vec<ShownMessage>,
}

impl Chat {
    pub fn new(chat_id: Option<[u8; 16]>) -> Chat {
        Chat {
            chat_id,
            messages: Vec::new(),
            loaded: false,
            more: true,
            loading: false,
            complete: false,
            pages: 0,
            asked_at: 0,
            marked: None,
            cleared: None,
            unsent: Vec::new(),
        }
    }

    /// The oldest seq shown.
    pub fn oldest(&self) -> Option<i64> {
        self.messages.iter().find_map(|m| m.seq)
    }
}

/// A tab's rows on screen, wrapped, each with its tone.
pub type Rows = Vec<(String, Tone)>;

/// What a tab's rows were built for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RowKey {
    pub tab: usize,
    pub chat: Option<Number>,
    pub width: u16,
    /// [`State::changes`] when they were built.
    pub changes: u64,
}

/// The open tab's rows as last built, so a frame that changes nothing
/// doesn't build them again. Kept by `view`, which can't change anything
/// else: a memo, not state.
#[derive(Debug, Default)]
pub struct RowCache(Mutex<Option<(RowKey, Arc<Rows>, usize)>>);

impl RowCache {
    /// The rows and the `/clear` row, if they were built for `key`.
    pub fn get(&self, key: &RowKey) -> Option<(Arc<Rows>, usize)> {
        let cached = self.0.lock().ok()?;
        cached.as_ref().filter(|(k, ..)| k == key).map(|(_, rows, clear)| (rows.clone(), *clear))
    }

    pub fn put(&self, key: RowKey, rows: Arc<Rows>, clear_row: usize) {
        if let Ok(mut cached) = self.0.lock() {
            *cached = Some((key, rows, clear_row));
        }
    }
}

/// A short message under the chat area that goes away by itself.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Flash {
    pub line: Line,
    pub ticks_left: u32,
}

pub struct State {
    pub screen: Screen,
    /// `corvo` first, then one per open chat.
    pub tabs: Vec<Tab>,
    pub current: usize,
    pub editor: Editor,
    pub history: History,
    /// The open tab's scroll; back to the bottom on every tab switch.
    pub scroll: Scroll,
    /// Your display name and number, for the status line.
    pub name: String,
    pub number: String,
    /// Your username, for the Profile tab.
    pub username: String,
    /// Your number, once on the main screen.
    pub me: Option<Number>,
    /// `None` until the connection first says.
    pub conn: Option<ConnState>,
    /// Everyone known here but you, as the core last listed them.
    pub people: Vec<PersonRow>,
    /// The chat list, newest first, as the core last listed it.
    pub list: Vec<ChatRow>,
    /// The messages of each open chat tab.
    pub chats: HashMap<Number, Chat>,
    /// What's been asked of the core and not answered yet.
    pub loads: Loads,
    /// Whether the window has focus: chats are only marked read while it does.
    pub focused: bool,
    /// The sidebar's first row shown, when it has more than fit.
    pub sidebar_top: usize,
    /// The chat the sidebar was last scrolled to show.
    pub sidebar_for: Option<Number>,
    pub flash: Option<Flash>,
    /// The terminal's size, in columns and rows.
    pub size: (u16, u16),
    /// The window title last asked for.
    pub title: String,
    /// What the app is doing while it can't take input, like "signing in…",
    /// or what it's waiting for the server to do, like "looking them up…".
    pub busy: Option<&'static str>,
    /// The call to the core that set `busy`: only its reply clears it.
    pub busy_call: Option<Command>,
    /// The completion list, while open.
    pub popup: Option<Popup>,
    /// Counts the changes that can change what a tab shows; `update` adds one
    /// for every message but the tick.
    pub changes: u64,
    pub rows: RowCache,
    /// A window over the main screen, while open.
    pub overlay: Option<Overlay>,
    /// Where your data lives, as `/about` shows it.
    pub data_dir: Option<String>,
    /// A command's question in the input box, while it waits for the answer.
    pub pending: Option<Pending>,
    /// The Profile tab's menu row marked `›`.
    pub profile_at: usize,
    /// People whose changed keys were already warned about.
    pub warned: HashSet<Number>,
    /// What to say about a newer release, once the check has found one.
    pub update_note: Option<String>,
}

impl State {
    pub fn new(name: String, number: String) -> State {
        State {
            screen: Screen::Main,
            tabs: vec![Tab::corvo()],
            current: 0,
            editor: Editor::default(),
            history: History::default(),
            scroll: Scroll::default(),
            name,
            number,
            username: String::new(),
            me: None,
            conn: None,
            people: Vec::new(),
            list: Vec::new(),
            chats: HashMap::new(),
            loads: Loads::default(),
            focused: true,
            sidebar_top: 0,
            sidebar_for: None,
            flash: None,
            size: (80, 24),
            title: String::new(),
            busy: None,
            busy_call: None,
            popup: None,
            changes: 0,
            rows: RowCache::default(),
            overlay: None,
            data_dir: None,
            pending: None,
            profile_at: 0,
            warned: HashSet::new(),
            update_note: None,
        }
    }

    /// Starts on the start screens.
    pub fn start(start: Start) -> State {
        State { screen: Screen::Start(Box::new(start)), ..State::new(String::new(), String::new()) }
    }

    /// The question the input box is answering, if it isn't a chat line.
    pub fn ask(&self) -> Option<Ask> {
        match &self.screen {
            Screen::Start(start) => Some(start.ask()),
            Screen::Main => self.pending.as_ref().map(Pending::ask),
        }
    }

    pub fn tab(&self) -> &Tab {
        &self.tabs[self.current]
    }

    /// Who the open tab's chat is with, if it's a chat.
    pub fn open_number(&self) -> Option<Number> {
        self.tab().number()
    }

    /// How `number` is shown: their display name when they share one that
    /// shows as something, else the number.
    pub fn label(&self, number: Number) -> String {
        let person = self.people.iter().find(|p| p.number == number).and_then(|p| p.name.as_deref());
        let row = || self.list.iter().find(|r| r.with == number).and_then(|r| r.name.as_deref());
        person
            .or_else(row)
            .filter(|name| !clean(&name.replace('\n', " ")).trim().is_empty())
            .map_or_else(|| number.to_string(), str::to_string)
    }

    /// The person with `number`, if known here.
    pub fn person(&self, number: Number) -> Option<&PersonRow> {
        self.people.iter().find(|p| p.number == number)
    }

    /// The tab for a chat with `number`, opened if needed; selected if
    /// `switch`. Whether it's new.
    pub fn open_chat(&mut self, number: Number, switch: bool) -> bool {
        let (at, new) = match self.tabs.iter().position(|t| t.number() == Some(number)) {
            Some(at) => (at, false),
            None => {
                self.tabs.push(Tab::chat(number, self.label(number)));
                self.chats.insert(number, Chat::new(None));
                (self.tabs.len() - 1, true)
            }
        };
        if switch {
            self.select_tab(at);
        }
        new
    }

    /// Titles the chat tabs again, after names may have changed.
    pub fn relabel(&mut self) {
        self.changes += 1;
        for i in 1..self.tabs.len() {
            if let Some(number) = self.tabs[i].number() {
                self.tabs[i].title = self.label(number);
            }
        }
    }

    /// Unread messages over every chat.
    pub fn unread_total(&self) -> usize {
        self.list.iter().filter(|r| r.relation != corvo_chat_proto::chat::Relation::Blocked).map(|r| r.unread).sum()
    }

    /// Messages of yours waiting to go (`○`): those in the open chats, and
    /// for the others, the last message when it's waiting.
    pub fn waiting(&self) -> usize {
        let waits = |m: &ShownMessage| m.mark == Some(Mark::Waiting);
        let open: usize = self.chats.values().map(|c| c.messages.iter().filter(|m| waits(m)).count()).sum();
        let others = self
            .list
            .iter()
            .filter(|r| !self.chats.values().any(|c| c.chat_id == Some(r.chat_id)))
            .filter(|r| r.last.as_ref().is_some_and(waits))
            .count();
        open + others
    }

    pub fn on_corvo(&self) -> bool {
        self.current == 0
    }

    pub fn on_profile(&self) -> bool {
        self.tab().kind == TabKind::Profile
    }

    /// Opens the Profile tab right after the corvo tab, or goes back to it.
    pub fn open_profile(&mut self) {
        let at = match self.tabs.iter().position(|t| t.kind == TabKind::Profile) {
            Some(at) => at,
            None => {
                self.tabs.insert(1, Tab::profile());
                self.profile_at = 0;
                if self.current >= 1 {
                    self.current += 1;
                }
                1
            }
        };
        self.select_tab(at);
    }

    /// Shows tab `idx`. Leaving the Profile tab closes it.
    pub fn select_tab(&mut self, mut idx: usize) {
        if idx >= self.tabs.len() {
            return;
        }
        if idx != self.current && self.on_profile() {
            self.tabs.remove(self.current);
            if idx > self.current {
                idx -= 1;
            }
        }
        self.current = idx;
        self.scroll.follow_end();
    }

    pub fn step_tab(&mut self, delta: isize) {
        let n = self.tabs.len() as isize;
        self.select_tab((self.current as isize + delta).rem_euclid(n) as usize);
    }

    /// Adds a line to the `corvo` tab, and flashes it if it's a warning or an error.
    pub fn say(&mut self, tone: Tone, text: impl Into<String>) {
        self.changes += 1;
        let line = Line { text: text.into(), tone };
        if matches!(tone, Tone::Warn | Tone::Bad) {
            self.flash = Some(Flash { line: line.clone(), ticks_left: FLASH_TICKS });
        }
        let corvo = &mut self.tabs[0];
        corvo.lines.push(line);
        if corvo.lines.len() > CORVO_LEN {
            corvo.lines.remove(0);
            corvo.cleared = corvo.cleared.saturating_sub(1);
        }
    }

    /// Like a terminal's `clear`: the open tab shows empty, and scrolling up
    /// shows what came before.
    pub fn clear_tab(&mut self) {
        self.changes += 1;
        match self.open_number() {
            Some(number) => {
                if let Some(chat) = self.chats.get_mut(&number) {
                    let newest = chat.messages.iter().chain(&chat.unsent).last();
                    chat.cleared = newest.map(|m| m.msg_id).or(chat.cleared);
                }
            }
            None => {
                let tab = &mut self.tabs[self.current];
                tab.cleared = tab.lines.len();
            }
        }
        self.scroll.follow_end();
    }

    /// Closes the chat tab with `number`, if one is open.
    pub fn close_chat(&mut self, number: Number) {
        let Some(at) = self.tabs.iter().position(|t| t.number() == Some(number)) else { return };
        if at == self.current {
            self.close_tab();
            return;
        }
        self.tabs.remove(at);
        self.chats.remove(&number);
        if at < self.current {
            self.current -= 1;
        }
        self.changes += 1;
    }

    /// Closes the open tab and shows the one before it. The `corvo` tab stays.
    pub fn close_tab(&mut self) -> bool {
        if self.on_corvo() {
            return false;
        }
        let tab = self.tabs.remove(self.current);
        if let Some(number) = tab.number() {
            self.chats.remove(&number);
        }
        self.current -= 1;
        self.scroll.follow_end();
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ed(text: &str) -> Editor {
        let mut e = Editor::default();
        e.set(text.into());
        e
    }

    #[test]
    fn editor_edits_and_moves_by_chars() {
        let mut e = Editor::default();
        for c in "héllo".chars() {
            e.insert(c);
        }
        e.left();
        e.left();
        e.backspace();
        assert_eq!(e.text(), "hélo");
        e.delete();
        assert_eq!(e.text(), "héo");
        e.home();
        e.insert('>');
        e.end();
        e.insert('!');
        assert_eq!(e.text(), ">héo!");
        assert_eq!(e.take(), ">héo!");
        assert!(e.is_empty());
    }

    #[test]
    fn newlines_rows_and_vertical_moves() {
        let mut e = ed("first line");
        e.newline();
        e.insert_str("second\r\nthird");
        assert_eq!(e.rows(), vec!["first line", "second", "third"]);
        assert_eq!(e.cursor_pos(), (2, 5));
        assert!(e.up());
        assert_eq!(e.cursor_pos(), (1, 5));
        assert!(e.up());
        assert_eq!(e.cursor_pos(), (0, 5));
        assert!(!e.up(), "already on the first row");
        assert!(e.down() && e.down());
        assert!(!e.down(), "already on the last row");
        e.home();
        assert_eq!(e.cursor_pos(), (2, 0));
    }

    #[test]
    fn pasted_tabs_and_lone_carriage_returns() {
        let mut e = ed("");
        e.insert_str("a\tb\r\n\tc\rd");
        assert_eq!(e.text(), "a    b\n    c\nd");
        assert_eq!(e.cursor_pos(), (2, 1));
    }

    #[test]
    fn ctrl_u_and_ctrl_w_stay_on_the_row() {
        let mut e = ed("keep\nsay hello  world");
        e.kill_word();
        assert_eq!(e.text(), "keep\nsay hello  ");
        e.kill_word();
        assert_eq!(e.text(), "keep\nsay ");
        e.kill_to_row_start();
        assert_eq!(e.text(), "keep\n");
        e.kill_to_row_start();
        assert_eq!(e.text(), "keep\n");
    }

    #[test]
    fn history_browses_and_keeps_the_draft() {
        let mut h = History::default();
        assert_eq!(h.prev("draft"), None);
        h.push("one");
        h.push("two");
        h.push("two");
        h.push("  ");
        assert_eq!(h.prev("draft").as_deref(), Some("two"));
        assert_eq!(h.prev("ignored").as_deref(), Some("one"));
        assert_eq!(h.prev("ignored"), None);
        assert_eq!(h.next().as_deref(), Some("two"));
        assert_eq!(h.next().as_deref(), Some("draft"));
        assert_eq!(h.next(), None);
        for i in 0..HISTORY_LEN + 5 {
            h.push(&format!("m{i}"));
        }
        let mut oldest = None;
        while let Some(e) = h.prev("") {
            oldest = Some(e);
        }
        assert_eq!(oldest.as_deref(), Some("m5"));
    }

    #[test]
    fn scrolling_pages_and_returns_to_the_bottom() {
        let mut s = Scroll::default();
        assert_eq!(s.top(100, 10), 90);
        s.page_up(100, 10);
        assert_eq!(s.top(100, 10), 80);
        assert_eq!(s.top(102, 10), 80, "new lines don't move the view");
        s.by(-500, 102, 10);
        assert_eq!(s.top(102, 10), 0);
        s.by(1000, 102, 10);
        assert!(!s.is_scrolled());
        s.page_up(5, 10);
        assert!(!s.is_scrolled(), "nothing to scroll when it all fits");
    }

    #[test]
    fn the_corvo_tab_keeps_its_newest_lines_and_its_clear_mark() {
        let mut state = State::new("ana".into(), "482 910 5573".into());
        for i in 0..CORVO_LEN {
            state.say(Tone::Plain, format!("line {i}"));
        }
        state.clear_tab();
        state.say(Tone::Plain, "after");
        let corvo = &state.tabs[0];
        assert_eq!(corvo.lines.len(), CORVO_LEN);
        assert_eq!(corvo.lines[0].text, "line 1");
        assert_eq!(corvo.cleared, CORVO_LEN - 1, "the mark moves with the lines");
        assert_eq!(corvo.lines[corvo.cleared].text, "after");
    }

    #[test]
    fn the_profile_tab_opens_after_corvo_and_closes_when_left() {
        let mut state = State::new("ana".into(), "482 910 5573".into());
        let (rahul, maria) = (Number::from_body(123_456_789).unwrap(), Number::from_body(222_333_444).unwrap());
        state.open_chat(rahul, false);
        state.open_chat(maria, true);
        state.open_profile();
        let kinds: Vec<TabKind> = state.tabs.iter().map(|t| t.kind).collect();
        assert_eq!(kinds, [TabKind::Corvo, TabKind::Profile, TabKind::Chat(rahul), TabKind::Chat(maria)]);
        assert!(state.on_profile());
        state.profile_at = 3;
        state.open_profile();
        assert_eq!((state.current, state.profile_at), (1, 3), "the same tab, as it was");
        state.select_tab(3);
        assert_eq!(state.open_number(), Some(maria), "the tab asked for, though the tabs moved");
        assert_eq!(state.tabs.len(), 3, "leaving it closes it");
        state.open_profile();
        state.close_chat(rahul);
        assert!(state.on_profile(), "closing a chat elsewhere doesn't close it");
    }

    #[test]
    fn only_warnings_and_errors_flash() {
        let mut state = State::new("ana".into(), "482 910 5573".into());
        state.say(Tone::Plain, "fine");
        assert_eq!(state.flash, None);
        state.say(Tone::Bad, "oops");
        assert_eq!(state.flash.as_ref().map(|f| f.line.text.as_str()), Some("oops"));
    }
}
