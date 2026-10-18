//! Windows over the main screen: the start art (with the tips for a new
//! account), the help and `/about`.

use std::path::Path;

use ratatui::crossterm::event::{KeyCode, KeyEvent};
use unicode_width::UnicodeWidthStr;

use super::commands::{HELP, KEYS};
use super::state::{APP, State, VERSION};
use super::view::wrap;

/// A window over the main screen, and how far down it's scrolled.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Overlay {
    /// At every start: the art and the version. For a new account
    /// (`first_run`) the tips too, and it stays until Enter or Esc;
    /// otherwise any key closes it, and it closes itself after `ticks_left`.
    Start {
        first_run: bool,
        ticks_left: u32,
        scroll: u16,
    },
    Help {
        scroll: u16,
    },
    /// `data_dir`: where your data lives, as shown.
    About {
        data_dir: String,
        scroll: u16,
    },
}

/// How long the start window stays without a key: 2 s of ticks.
pub const START_TICKS: u32 = 8;

/// The help's columns: the group, then the command (or key), then what it
/// does. Narrower than `GROUP + COMMAND + DESCRIPTION`, each group's name
/// goes on a row of its own instead.
const GROUP: usize = 11;
const COMMAND: usize = 18;
const DESCRIPTION: usize = 12;
/// Columns a window's text leaves for its border and margins: two at the
/// left, at least two at the right.
const FRAME: u16 = 6;

/// Where the source is, for `/about`.
const SOURCE: &str = env!("CARGO_PKG_REPOSITORY");

/// "corvo", for the start window and the goodbye.
pub const ART: &[&str] = &[
    r"  ___   ___   _ __ __   __  ___",
    r" / __| / _ \ | '__|\ \ / / / _ \",
    r"| (__ | (_) || |    \ V / | (_) |",
    r" \___| \___/ |_|     \_/   \___/",
];

/// Printed in the terminal after quitting.
pub fn goodbye() -> Vec<&'static str> {
    ART.iter().copied().chain(["", "  bye for now"]).collect()
}

const WELCOME: &[&str] = &[
    "Chat with friends, end to end encrypted.",
    "",
    "• Your number is how people find you: /code shows it.",
    "• To add a friend: /add <their number>, then say hi.",
    "• Marks: ○ waiting  · sent  ✓ received  ✓✓ read  ✗ refused",
    "• The server stores only what it can't read.",
    "",
    "            /help any time · Enter to start",
];

impl Overlay {
    /// The start window for a new account, or for an unlock.
    pub fn start(first_run: bool) -> Overlay {
        Overlay::Start { first_run, ticks_left: START_TICKS, scroll: 0 }
    }

    pub fn title(&self) -> &'static str {
        match self {
            Overlay::Start { first_run: true, .. } => "─ Welcome to corvo ",
            Overlay::Start { .. } => "─ corvo ",
            Overlay::Help { .. } => "─ Help ",
            Overlay::About { .. } => "─ About ",
        }
    }

    /// What the top border says at its right end, if anything.
    pub fn hint(&self) -> Option<&'static str> {
        match self {
            Overlay::Start { .. } => None,
            Overlay::Help { .. } | Overlay::About { .. } => Some(" Esc closes ─"),
        }
    }

    /// The widest it gets: its text, the margins and the border.
    pub fn width(&self) -> u16 {
        match self {
            Overlay::Start { first_run: true, .. } => 64,
            Overlay::Start { .. } => 40,
            Overlay::Help { .. } => 80,
            Overlay::About { .. } => 64,
        }
    }

    pub fn scroll(&self) -> u16 {
        match self {
            Overlay::Start { scroll, .. } | Overlay::Help { scroll } | Overlay::About { scroll, .. } => *scroll,
        }
    }

    fn set_scroll(&mut self, to: u16) {
        match self {
            Overlay::Start { scroll, .. } | Overlay::Help { scroll } | Overlay::About { scroll, .. } => *scroll = to,
        }
    }
}

/// The window's text as drawn on a terminal of `size`: wrapped to fit
/// when the terminal is narrower than the window.
pub fn rows(overlay: &Overlay, size: (u16, u16)) -> Vec<String> {
    let inner = usize::from(size.0.min(overlay.width()).saturating_sub(FRAME));
    let wrapped = |lines: &[String]| lines.iter().flat_map(|line| wrap(line, inner)).collect::<Vec<_>>();
    match overlay {
        Overlay::Start { first_run, .. } => {
            // The art is never wrapped: at its narrowest the window still fits it.
            let mut rows: Vec<String> = ART.iter().map(|r| r.to_string()).collect();
            rows.push(format!("{:^w$}", format!("{APP} v{VERSION}"), w = art_width()));
            if *first_run {
                rows.push(String::new());
                rows.extend(wrapped(&WELCOME.iter().map(|l| l.to_string()).collect::<Vec<_>>()));
            }
            rows.iter().map(|r| r.trim_end().to_string()).collect()
        }
        Overlay::Help { .. } => help_rows(inner),
        Overlay::About { data_dir, .. } => wrapped(&[
            format!("{APP} v{VERSION}"),
            "End-to-end encrypted chat in your terminal.".into(),
            String::new(),
            format!("Your data: {data_dir}"),
            format!("Source:    {}", SOURCE.trim_start_matches("https://")),
            "License MIT".into(),
        ]),
    }
}

fn art_width() -> usize {
    ART.iter().map(|r| r.width()).max().unwrap_or(0)
}

/// The help in `inner` columns: each group with its commands and what they
/// do, which wraps under itself; then the keys.
fn help_rows(inner: usize) -> Vec<String> {
    let wide = inner >= GROUP + COMMAND + DESCRIPTION;
    let indent = if wide { GROUP } else { 2 };
    let mut rows = Vec::new();
    for (group, items) in HELP.iter().chain([("Keys", KEYS)].iter()) {
        if !wide {
            rows.push(group.to_string());
        }
        for (i, (name, what)) in items.iter().enumerate() {
            let lead = if wide && i == 0 { format!("{group:<GROUP$}") } else { " ".repeat(indent) };
            // A long key gets two spaces after it, past the column.
            let column = COMMAND.max(name.width() + 2);
            let width = inner.saturating_sub(indent + column).max(DESCRIPTION);
            for (j, part) in wrap(what, width).into_iter().enumerate() {
                let head = if j == 0 {
                    format!("{lead}{name}{}", " ".repeat(column - name.width()))
                } else {
                    " ".repeat(indent + column)
                };
                rows.push(format!("{head}{part}"));
            }
        }
    }
    rows
}

/// How many of `total` rows the window shows at once: all of them if there's
/// room, leaving a row above and below it.
pub fn height(size: (u16, u16), total: usize) -> u16 {
    u16::try_from(total).unwrap_or(u16::MAX).min(size.1.saturating_sub(4)).max(1)
}

/// How far down the window shows at `size`: as scrolled, but never past its
/// end, which moves when the terminal is resized.
pub fn scroll(overlay: &Overlay, size: (u16, u16)) -> u16 {
    overlay.scroll().min(last(size, rows(overlay, size).len()))
}

/// The furthest `total` rows can scroll at `size`.
fn last(size: (u16, u16), total: usize) -> u16 {
    u16::try_from(total).unwrap_or(u16::MAX).saturating_sub(height(size, total))
}

/// Whether the window has rows below what it shows at `size`.
pub fn more_below(overlay: &Overlay, size: (u16, u16)) -> bool {
    scroll(overlay, size) < last(size, rows(overlay, size).len())
}

/// A key while a window is open. The arrows and PgUp/PgDn scroll it. Then:
/// the start window after an unlock closes on any key; the rest close on
/// Enter or Esc.
pub fn key(state: &mut State, key: KeyEvent) {
    let Some(overlay) = &state.overlay else { return };
    let page = height(state.size, rows(overlay, state.size).len()) as isize;
    let any_key_closes = matches!(overlay, Overlay::Start { first_run: false, .. });
    match key.code {
        _ if any_key_closes => state.overlay = None,
        KeyCode::Up => scroll_by(state, -1),
        KeyCode::Down => scroll_by(state, 1),
        KeyCode::PageUp => scroll_by(state, -page),
        KeyCode::PageDown => scroll_by(state, page),
        KeyCode::Esc | KeyCode::Enter => state.overlay = None,
        _ => {}
    }
}

/// A tick: the start window after an unlock closes itself when its time is up.
pub fn tick(state: &mut State) {
    if let Some(Overlay::Start { first_run: false, ticks_left, .. }) = &mut state.overlay {
        *ticks_left = ticks_left.saturating_sub(1);
        if *ticks_left == 0 {
            state.overlay = None;
        }
    }
}

/// Scrolls the open window by `delta` rows, within its text.
pub fn scroll_by(state: &mut State, delta: isize) {
    let size = state.size;
    let Some(overlay) = &mut state.overlay else { return };
    let last = last(size, rows(overlay, size).len());
    let to = (scroll(overlay, size) as isize).saturating_add(delta).clamp(0, last as isize);
    overlay.set_scroll(u16::try_from(to).unwrap_or(last));
}

/// `path` with the home folder shown as `~`.
pub fn shown_path(path: &Path, home: Option<&Path>) -> String {
    match home.and_then(|home| path.strip_prefix(home).ok()) {
        Some(rest) => Path::new("~").join(rest).display().to_string(),
        None => path.display().to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ui::state::State;
    use crate::ui::update::{Effect, Msg, update};
    use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

    fn key(state: &mut State, code: KeyCode) -> Vec<Effect> {
        update(state, Msg::Key(KeyEvent::new(code, KeyModifiers::NONE)))
    }

    fn send_line(state: &mut State, text: &str) {
        for c in text.chars() {
            key(state, KeyCode::Char(c));
        }
        key(state, KeyCode::Enter);
    }

    fn main_screen() -> State {
        let mut state = State::new("ana".into(), "482 910 5573".into());
        state.size = (80, 24);
        state
    }

    fn first_run() -> Overlay {
        Overlay::start(true)
    }

    #[test]
    fn the_start_window_closes_itself_after_two_seconds() {
        let mut s = main_screen();
        s.overlay = Some(Overlay::start(false));
        assert_eq!(s.overlay, Some(Overlay::Start { first_run: false, ticks_left: START_TICKS, scroll: 0 }));
        for _ in 0..START_TICKS - 1 {
            update(&mut s, Msg::Tick);
        }
        assert!(s.overlay.is_some(), "still up just before 2 s");
        update(&mut s, Msg::Tick);
        assert_eq!(s.overlay, None);
        let shown = rows(&Overlay::start(false), (80, 24));
        assert_eq!(&shown[..ART.len()], ART);
        assert!(shown.iter().any(|r| r.trim() == format!("corvo-chat v{VERSION}")), "{shown:#?}");
        assert!(!shown.iter().any(|r| r.contains("/help any time")), "no tips after an unlock");
    }

    #[test]
    fn for_a_new_account_the_start_window_shows_the_tips_and_stays() {
        let mut s = main_screen();
        s.overlay = Some(first_run());
        for _ in 0..START_TICKS * 2 {
            update(&mut s, Msg::Tick);
        }
        assert_eq!(s.overlay, Some(first_run()), "it doesn't close itself");
        let shown = rows(&first_run(), (80, 24));
        assert_eq!(&shown[..ART.len()], ART);
        for line in WELCOME.iter().filter(|l| !l.is_empty()) {
            assert!(shown.iter().any(|r| r == line), "{line:?} as it is: {shown:#?}");
        }
        key(&mut s, KeyCode::Char('x'));
        assert!(s.overlay.is_some(), "only Enter or Esc close it");
        key(&mut s, KeyCode::Enter);
        assert_eq!(s.overlay, None);
    }

    #[test]
    fn any_key_closes_the_start_window_but_ctrl_c_quits() {
        for code in [KeyCode::Char('h'), KeyCode::Enter, KeyCode::Esc, KeyCode::Down] {
            let mut s = main_screen();
            s.overlay = Some(Overlay::start(false));
            key(&mut s, code);
            assert_eq!(s.overlay, None, "{code:?}");
            assert_eq!(s.editor.text(), "", "{code:?} only closes it");
        }
        let mut s = main_screen();
        s.overlay = Some(Overlay::start(false));
        let ctrl_c = update(&mut s, Msg::Key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL)));
        assert!(ctrl_c.contains(&Effect::Quit), "{ctrl_c:?}");
    }

    #[test]
    fn keys_typed_while_the_tips_are_open_do_not_reach_the_input() {
        let mut s = main_screen();
        s.overlay = Some(first_run());
        for c in "hi".chars() {
            key(&mut s, KeyCode::Char(c));
        }
        assert_eq!(s.editor.text(), "");
        assert!(s.overlay.is_some());
    }

    #[test]
    fn the_tips_scroll_only_as_far_as_they_need() {
        let mut s = main_screen();
        s.overlay = Some(first_run());
        key(&mut s, KeyCode::Down);
        assert_eq!(s.overlay, Some(first_run()), "it fits on 80×24");

        s.size = (80, 10);
        for _ in 0..3 {
            key(&mut s, KeyCode::PageDown);
        }
        let Some(overlay @ Overlay::Start { scroll, .. }) = s.overlay.clone() else { panic!("closed") };
        let total = rows(&overlay, s.size).len();
        assert_eq!(usize::from(scroll), total - usize::from(height(s.size, total)), "at the end, no further");
        key(&mut s, KeyCode::Up);
        assert_eq!(s.overlay.as_ref().map(Overlay::scroll), Some(scroll - 1));
    }

    #[test]
    fn about_opens_a_window_and_esc_closes_it() {
        let mut s = main_screen();
        s.data_dir = Some("~/.config/corvo-chat".into());
        send_line(&mut s, "/about");
        let Some(overlay @ Overlay::About { .. }) = s.overlay.clone() else { panic!("{:?}", s.overlay) };
        let shown = rows(&overlay, (80, 24));
        assert_eq!(shown[0], format!("corvo-chat v{VERSION}"));
        assert!(shown.iter().any(|r| r == "Your data: ~/.config/corvo-chat"), "{shown:#?}");
        assert!(shown.iter().any(|r| r == "Source:    github.com/Abinsanth/corvo-chat-client"), "{shown:#?}");
        assert!(shown.iter().any(|r| r == "License MIT"));
        key(&mut s, KeyCode::Esc);
        assert_eq!(s.overlay, None);
    }

    #[test]
    fn the_art_fits_the_smallest_terminal() {
        assert!(ART.len() <= 5);
        assert!(art_width() <= 34, "{}", art_width());
        assert!(goodbye().iter().all(|r| r.width() <= 40));
        assert_eq!(goodbye().last(), Some(&"  bye for now"));
        let shown = rows(&Overlay::start(false), (40, 12));
        assert_eq!(&shown[..ART.len()], ART, "not wrapped at 40 columns");
    }

    #[test]
    fn the_data_folder_is_shown_from_home() {
        let home = Path::new("/home/ana");
        let shown = shown_path(&home.join(".config").join("corvo-chat"), Some(home));
        // With the system's own separator: `~\.config\corvo-chat` on Windows.
        assert_eq!(shown, Path::new("~").join(".config").join("corvo-chat").display().to_string());
        assert!(shown.starts_with('~') && !shown.contains("home"));
        assert_eq!(shown_path(Path::new("/srv/corvo"), Some(home)), "/srv/corvo");
        assert_eq!(shown_path(Path::new("/srv/corvo"), None), "/srv/corvo");
    }

    #[test]
    fn help_opens_a_window_and_prints_nothing() {
        for line in ["/help", "/h"] {
            let mut s = main_screen();
            let said = s.tabs[0].lines.len();
            send_line(&mut s, line);
            assert_eq!(s.overlay, Some(Overlay::Help { scroll: 0 }));
            assert_eq!(s.tabs[0].lines.len(), said, "nothing printed into the corvo tab");
            key(&mut s, KeyCode::Esc);
            assert_eq!(s.overlay, None);
        }
    }

    #[test]
    fn help_lists_every_command_in_the_final_set() {
        let help = help_rows(74);
        let narrow = help_rows(34);
        for name in [
            "/code",
            "/add",
            "/invite",
            "/accept",
            "/reject",
            "/block",
            "/unblock",
            "/remove",
            "/verify",
            "/report",
            "/info",
            "/chat",
            "/contacts",
            "/requests",
            "/blocked",
            "/profile",
            "/close",
            "/clear",
            "/about",
            "/help",
            "/quit",
        ] {
            for rows in [&help, &narrow] {
                let found = rows.iter().filter(|r| r.split_whitespace().any(|word| word == name)).count();
                assert_eq!(found, 1, "{name} once in the help: {rows:#?}");
            }
        }
    }

    #[test]
    fn the_help_scrolls_on_a_short_terminal() {
        let mut s = main_screen();
        s.size = (80, 12);
        s.overlay = Some(Overlay::Help { scroll: 0 });
        for _ in 0..20 {
            key(&mut s, KeyCode::PageDown);
        }
        let Some(overlay @ Overlay::Help { scroll }) = s.overlay.clone() else { panic!("closed") };
        let total = rows(&overlay, s.size).len();
        assert!(scroll > 0);
        assert_eq!(usize::from(scroll), total - usize::from(height(s.size, total)));
    }

    #[test]
    fn a_long_help_line_wraps_under_its_description() {
        let help = help_rows(GROUP + COMMAND + DESCRIPTION);
        let at = help.iter().position(|r| r.contains("/contacts")).expect("listed");
        assert!(help[at].ends_with("  contacts and"), "{:?}", &help[at..at + 2]);
        let indent = help[at].find("contacts and").expect("described");
        assert_eq!(help[at + 1], format!("{}requests", " ".repeat(indent)));
    }

    #[test]
    fn the_help_reads_as_decided() {
        let help = help_rows(74);
        for row in [
            "You        /profile          name, password, devices",
            "Friends    /code             your number",
            "           /add <number>     add a friend",
            "           /verify <name>    compare safety codes",
            "Chatting   /chat <name>      open a chat",
            "App        /about            version and info",
            "           /quit  /q         quit (Ctrl-C)",
            "Keys       Tab               complete",
            "           Shift-Tab, Alt-1..9  switch tabs",
            "           ↑/↓               what you typed before",
        ] {
            assert!(help.iter().any(|r| r == row), "{row:?} missing: {help:#?}");
        }
    }

    #[test]
    fn a_narrow_help_puts_each_group_on_its_own_row() {
        let help = help_rows(34);
        assert_eq!(help[0], "You");
        assert!(help[1].starts_with("  /profile"), "{help:#?}");
        assert!(help.iter().any(|r| r == "Keys"));
        assert!(help.iter().all(|r| r.width() <= 34), "{help:#?}");
    }

    #[test]
    fn the_wheel_scrolls_a_window_and_clicks_do_nothing() {
        use crate::ui::state::WHEEL_STEP;
        use ratatui::crossterm::event::{MouseButton, MouseEvent, MouseEventKind};
        let mouse = |kind| Msg::Mouse(MouseEvent { kind, column: 30, row: 10, modifiers: KeyModifiers::NONE });
        let mut s = main_screen();
        s.overlay = Some(Overlay::Help { scroll: 0 });
        update(&mut s, mouse(MouseEventKind::ScrollDown));
        assert_eq!(s.overlay, Some(Overlay::Help { scroll: WHEEL_STEP as u16 }));
        update(&mut s, mouse(MouseEventKind::ScrollUp));
        assert_eq!(s.overlay, Some(Overlay::Help { scroll: 0 }));
        let tab = s.current;
        update(&mut s, mouse(MouseEventKind::Down(MouseButton::Left)));
        assert_eq!(s.overlay, Some(Overlay::Help { scroll: 0 }));
        assert_eq!(s.current, tab);
    }

    #[test]
    fn a_bigger_terminal_brings_a_scrolled_window_back_into_view() {
        let mut s = main_screen();
        let scrolled = || Overlay::Start { first_run: true, ticks_left: START_TICKS, scroll: 13 };
        s.overlay = Some(scrolled());
        key(&mut s, KeyCode::Up);
        assert_eq!(s.overlay, Some(first_run()), "it all fits on 80×24 now");
        s.overlay = Some(scrolled());
        key(&mut s, KeyCode::Down);
        assert_eq!(s.overlay, Some(first_run()));
    }
}
