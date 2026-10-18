//! Drawing. Reads the state and changes nothing.
//!
//! ```text
//!  corvo │ rahul │ ana                      tabs
//! ● rahul        │ hi!                      sidebar │ chat area
//! ○ ana          │
//!                │
//! Requests (1)   │
//!   482 910 5573 │
//! ! that number has a typo                  flash
//! ────────────────────────────────────────
//! > hello_                                  input (1 to 5 rows)
//!  Ana · 482 910 5573 · ● online            status line
//! ```

use std::iter::Peekable;
use std::str::Chars;
use std::sync::Arc;

use corvo_chat_core::{ConnState, Mark};
use corvo_chat_proto::Number;
use corvo_chat_proto::chat::Relation;
use ratatui::Frame;
use ratatui::layout::{Position, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line as TextLine, Span};
use ratatui::widgets::{Block, Borders, Clear, Paragraph, Wrap};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use super::chat::local;
use super::start::Start;
use super::state::{APP, Line, MAX_INPUT_ROWS, RowKey, Rows, Screen, State, Tab, TabKind, Tone, VERSION};
use super::window::{self, Overlay};

/// Smaller than this, only [`TOO_SMALL`] is drawn.
pub const MIN_WIDTH: u16 = 40;
pub const MIN_HEIGHT: u16 = 12;
pub const TOO_SMALL: &str = "make the window bigger";
/// Rows that aren't the chat area or the input: tabs, flash, the input's border, status.
const FIXED_ROWS: u16 = 4;
/// Before the input's first row, and the space before its others.
const PROMPT: &str = "> ";
/// What a masked answer shows for each character.
const MASK: char = '•';
/// Most completion choices shown at once; the list scrolls with the selection.
pub const POPUP_ROWS: usize = 6;

/// Where each part of the screen goes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Areas {
    pub tabs: Rect,
    pub sidebar: Rect,
    pub chat: Rect,
    pub flash: Rect,
    pub border: Rect,
    pub input: Rect,
    pub status: Rect,
}

/// The layout for a terminal of `size`; `None` when it's too small.
pub fn areas(size: (u16, u16), input_rows: usize) -> Option<Areas> {
    let (w, h) = size;
    if w < MIN_WIDTH || h < MIN_HEIGHT {
        return None;
    }
    let input_h = input_rows.clamp(1, MAX_INPUT_ROWS) as u16;
    let body_h = h - FIXED_ROWS - input_h;
    let side_w = (w / 4).clamp(14, 24);
    let row = |y, height| Rect::new(0, y, w, height);
    Some(Areas {
        tabs: row(0, 1),
        sidebar: Rect::new(0, 1, side_w, body_h),
        chat: Rect::new(side_w + 1, 1, w - side_w - 1, body_h),
        flash: row(1 + body_h, 1),
        border: row(2 + body_h, 1),
        input: row(3 + body_h, input_h),
        status: row(h - 1, 1),
    })
}

/// How many rows the input box shows.
pub fn input_rows(state: &State) -> usize {
    state.editor.rows().len().clamp(1, MAX_INPUT_ROWS)
}

/// Which side of a chat a line is on: theirs and everything else on the
/// left, yours on the right.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Side {
    Left,
    Right,
}

/// One line of a tab. `head` is, on the left, the bytes of `text` before a
/// message's own text (its time), and on the right the bytes after it (its
/// time and mark); 0 for lines that aren't messages.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Shown {
    pub line: Line,
    pub head: usize,
    pub side: Side,
}

impl Shown {
    fn plain(line: Line) -> Shown {
        Shown { line, head: 0, side: Side::Left }
    }
}

/// A mark in two columns, so a line stays still as it changes.
fn mark_slot(mark: Option<&Mark>) -> &'static str {
    match mark {
        Some(Mark::Waiting) => "○ ",
        Some(Mark::Sent) | None => "· ",
        Some(Mark::Received) => "✓ ",
        Some(Mark::Read) => "✓✓",
        Some(Mark::Refused(_)) => "✗ ",
    }
}

/// What a tab shows, and the line its last `/clear` was at. A chat tab's
/// lines come from its messages: a line for each new day, then theirs on the
/// left (`14:02 text`) and yours on the right (`text 14:02 ✓✓`). No names:
/// the tab says who it is.
pub fn tab_lines(state: &State, tab: &Tab) -> (Vec<Shown>, usize) {
    let number = match tab.kind {
        TabKind::Corvo => return (tab.lines.iter().cloned().map(Shown::plain).collect(), tab.cleared),
        TabKind::Chat(number) => number,
    };
    let label = state.label(number);
    let mut lines = Vec::new();
    let person = state.person(number);
    let deleted = person.map(|p| p.deleted).or_else(|| state.list.iter().find(|r| r.with == number).map(|r| r.deleted));
    if deleted == Some(true) {
        lines.push(Shown::plain(Line { text: "account deleted".into(), tone: Tone::Warn }));
    } else if person.is_some_and(|p| p.relation == Relation::Request) {
        let text = format!("{label} isn't a contact yet: /accept adds them, /block stops them");
        lines.push(Shown::plain(Line { text, tone: Tone::Warn }));
    }
    let Some(chat) = state.chats.get(&number) else { return (lines, 0) };
    let mut clear_line = None;
    let mut last_day = None;
    for m in &chat.messages {
        let at = local(m.sent_at_ms);
        let day = at.map(|t| t.date_naive());
        if day != last_day {
            let text = at.map_or_else(|| "── ? ──".into(), |t| format!("── {} ──", t.format("%a %-d %b")));
            lines.push(Shown::plain(Line { text, tone: Tone::Dim }));
            last_day = day;
        }
        let time = at.map_or_else(|| "--:--".into(), |t| t.format("%H:%M").to_string());
        if m.mine {
            let tail = format!(" {time} {}", mark_slot(m.mark.as_ref()));
            let text = m.text.as_deref().unwrap_or("⚠ couldn't decrypt this message");
            let line = Line { text: format!("{text}{tail}"), tone: Tone::Own };
            lines.push(Shown { line, head: tail.len(), side: Side::Right });
            if let Some(Mark::Refused(why)) = &m.mark {
                let line = Line { text: format!("✗ not sent: {why}"), tone: Tone::Bad };
                lines.push(Shown { line, head: 0, side: Side::Right });
            }
        } else {
            let head = format!("{time} ");
            let (text, tone) = match &m.text {
                Ok(text) => (text.clone(), Tone::Plain),
                Err(_) => (format!("⚠ couldn't decrypt a message from {label}"), Tone::Warn),
            };
            let line = Line { text: format!("{head}{text}"), tone };
            lines.push(Shown { line, head: head.len(), side: Side::Left });
        }
        if chat.cleared == Some(m.msg_id) {
            clear_line = Some(lines.len());
        }
    }
    (lines, clear_line.unwrap_or(0))
}

#[cfg(test)]
thread_local! {
    /// How many times a tab's rows were built, for tests.
    static BUILDS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

/// The open tab's rows at `width`, and the row its last `/clear` was at.
/// Built once per change and width, then reused.
pub fn chat_rows(state: &State, width: u16) -> (Arc<Rows>, usize) {
    let key = RowKey { tab: state.current, chat: state.open_number(), width, changes: state.changes };
    if let Some(hit) = state.rows.get(&key) {
        return hit;
    }
    let (rows, clear_row) = build_rows(state, state.tab(), width);
    let rows = Arc::new(rows);
    state.rows.put(key, rows.clone(), clear_row);
    (rows, clear_row)
}

fn build_rows(state: &State, tab: &Tab, width: u16) -> (Rows, usize) {
    #[cfg(test)]
    BUILDS.with(|b| b.set(b.get() + 1));
    let (lines, cleared) = tab_lines(state, tab);
    let mut rows = Vec::new();
    let mut clear_row = None;
    for (i, shown) in lines.iter().enumerate() {
        if i == cleared {
            clear_row = Some(rows.len());
        }
        let tone = shown.line.tone;
        rows.extend(line_rows(&shown.line.text, shown.head, width.into(), shown.side).into_iter().map(|r| (r, tone)));
    }
    let clear_row = clear_row.unwrap_or(rows.len());
    (rows, clear_row)
}

/// How wide a message gets in a chat `width` columns wide: about 70% of it,
/// but at least 30 columns where there's room.
fn bubble(width: usize) -> usize {
    (width * 7 / 10).max(width.min(30))
}

/// One line's rows in a chat `width` columns wide, each cleaned. A message
/// wraps within [`bubble`]; other lines take the whole width.
///
/// On the left, after its first `head` bytes, the text wraps with a hanging
/// indent, so every row of it starts under its first column (not when the
/// head takes more than half the room). On the right, the rows are a block
/// against the right edge, lined up with each other on the left, and the
/// last `head` bytes (time and mark) follow the text on its last row. Rows on
/// the right come back padded to `width`.
fn line_rows(text: &str, head: usize, width: usize, side: Side) -> Vec<String> {
    match side {
        Side::Left if head > 0 => left_rows(text, head, bubble(width)),
        Side::Left => left_rows(text, 0, width),
        Side::Right => right_rows(text, head, width),
    }
}

fn left_rows(text: &str, head: usize, width: usize) -> Vec<String> {
    let indent = text.get(..head).map_or(0, |h| h.width());
    let (lead, body, indent) =
        if head > 0 && indent * 2 <= width { (&text[..head], &text[head..], indent) } else { ("", text, 0) };
    let mut rows = Vec::new();
    for piece in body.split('\n') {
        for row in wrap(&clean(piece), width - indent) {
            let before = if rows.is_empty() { lead.to_string() } else { " ".repeat(indent) };
            rows.push(format!("{before}{row}"));
        }
    }
    rows
}

fn right_rows(text: &str, tail: usize, width: usize) -> Vec<String> {
    let room = bubble(width);
    let (body, tail) = text.split_at(text.len().saturating_sub(tail));
    let mut rows: Vec<String> = body.split('\n').flat_map(|piece| wrap(&clean(piece), room)).collect();
    let last = rows.last_mut().expect("wrap gives a row at least");
    // The time and mark go after the text, or on a row of their own, flush
    // with the block's right edge.
    let alone = last.width() + tail.width() > room;
    if alone {
        rows.push(tail.trim_start().to_string());
    } else {
        last.push_str(tail);
    }
    let block = rows.iter().map(|r| r.width()).max().unwrap_or(0).min(width);
    let pad = |n: usize| " ".repeat(n);
    let count = rows.len();
    rows.into_iter()
        .enumerate()
        .map(|(i, r)| {
            let spare = block.saturating_sub(r.width());
            if alone && i + 1 == count {
                format!("{}{}{r}", pad(width - block), pad(spare))
            } else {
                format!("{}{r}{}", pad(width - block), pad(spare))
            }
        })
        .collect()
}

/// (rows to scroll through, rows visible) in the chat area. After a `/clear`
/// there are always enough rows below the mark to fill the area, so it starts
/// out empty.
pub fn chat_page(state: &State) -> (usize, usize) {
    let Some(areas) = areas(state.size, input_rows(state)) else {
        return (0, 0);
    };
    let (rows, clear_row) = chat_rows(state, areas.chat.width);
    let height = areas.chat.height as usize;
    (scroll_total(rows.len(), clear_row, height), height)
}

/// Rows to scroll through: after a `/clear`, enough below its mark to fill the area.
fn scroll_total(rows: usize, clear_row: usize, height: usize) -> usize {
    rows.max(clear_row + height)
}

/// Characters that show nothing, or turn the text around them backwards, so
/// text could pass for something else. The zero-width joiners U+200C and
/// U+200D, variation selectors and the tag characters of subdivision flags
/// aren't here, because emoji and Indic and Persian writing need them.
fn is_invisible(c: char) -> bool {
    matches!(
        c,
        '\u{00AD}' // soft hyphen
            | '\u{034F}' // combining grapheme joiner
            | '\u{061C}' // Arabic letter mark
            | '\u{115F}' | '\u{1160}' | '\u{3164}' | '\u{FFA0}' // Hangul fillers
            | '\u{17B4}' | '\u{17B5}' // Khmer inherent vowels
            | '\u{180B}'..='\u{180F}' // Mongolian variation selectors and vowel separator
            | '\u{200B}' // zero-width space
            | '\u{200E}' | '\u{200F}' // left-to-right and right-to-left marks
            | '\u{202A}'..='\u{202E}' // direction embeddings and overrides
            | '\u{2060}'..='\u{206F}' // word joiner, invisible operators, direction isolates
            | '\u{FEFF}' // byte-order mark
            | '\u{FFF9}'..='\u{FFFB}' // interlinear annotation
            | '\u{2800}' // braille blank
            | '\u{1BCA0}'..='\u{1BCA3}' // shorthand format controls
            | '\u{1D173}'..='\u{1D17A}' // musical formatting
            | '\u{E0001}' // language tag
    )
}

/// Text from someone else, made safe to draw: escape sequences, control
/// characters and invisible or text-direction characters are removed, tabs
/// become spaces. Line breaks go too, so split lines before cleaning them.
/// Zero-width joiners and variation selectors stay, since emoji need them.
pub fn clean(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\x1b' => skip_escape(&mut chars),
            '\t' => out.push(' '),
            c if c.is_control() || is_invisible(c) => {}
            c => out.push(c),
        }
    }
    out
}

/// Skips the rest of an escape sequence whose ESC was just read.
fn skip_escape(chars: &mut Peekable<Chars>) {
    match chars.next() {
        // CSI: parameter and intermediate bytes, then one final byte.
        Some('[') => {
            for c in chars.by_ref() {
                if !('\x20'..='\x3f').contains(&c) {
                    break;
                }
            }
        }
        // OSC and the other string sequences: up to BEL or ESC \.
        Some(']' | 'P' | 'X' | '^' | '_') => {
            while let Some(c) = chars.next() {
                if c == '\x07' {
                    break;
                }
                if c == '\x1b' {
                    chars.next_if_eq(&'\\');
                    break;
                }
            }
        }
        // Everything else is ESC and one more character.
        _ => {}
    }
}

/// Splits `text` into rows at most `width` columns wide, at spaces when it
/// can. Wide characters count as two columns.
pub fn wrap(text: &str, width: usize) -> Vec<String> {
    let width = width.max(1);
    let mut rows = Vec::new();
    let mut row = String::new();
    let mut row_w = 0;
    for c in text.chars() {
        // What it adds to the row as drawn: a variation selector can make the
        // character before it wider (❤ is one column, ❤️ two).
        row.push(c);
        let w = row.width().saturating_sub(row_w);
        row.pop();
        if row_w + w > width && !row.is_empty() {
            match row.rfind(' ') {
                // A space that doesn't fit is where the row ends.
                _ if c == ' ' => rows.push(std::mem::take(&mut row)),
                Some(i) if i > 0 => {
                    let rest = row[i + 1..].to_string();
                    rows.push(row[..i].trim_end().to_string());
                    row = rest;
                }
                _ => rows.push(std::mem::take(&mut row)),
            }
            row_w = row.width();
            if row_w + w > width && !row.is_empty() {
                rows.push(std::mem::take(&mut row));
                row_w = 0;
            }
        }
        if c == ' ' && row.is_empty() && !rows.is_empty() {
            continue;
        }
        row.push(c);
        row_w = row.width();
    }
    rows.push(row);
    rows
}

pub fn draw(frame: &mut Frame, state: &State) {
    let area = frame.area();
    if let Screen::Start(start) = &state.screen {
        draw_start(frame, state, start, area);
        return;
    }
    let Some(areas) = areas((area.width, area.height), input_rows(state)) else {
        frame.render_widget(Paragraph::new(TOO_SMALL).wrap(Wrap { trim: true }), area);
        return;
    };
    draw_tabs(frame, state, areas.tabs);
    draw_sidebar(frame, state, areas.sidebar);
    draw_chat(frame, state, areas.chat);
    draw_flash(frame, state, areas.flash);
    frame.render_widget(Block::new().borders(Borders::TOP).border_style(dim()), areas.border);
    draw_input(frame, state, areas.input);
    draw_popup(frame, state, areas.input);
    draw_status(frame, state, areas.status);
    if let Some(overlay) = &state.overlay {
        draw_overlay(frame, overlay, area);
    }
}

/// A window centred over the screen, scrolled as `update` left it.
fn draw_overlay(frame: &mut Frame, overlay: &Overlay, area: Rect) {
    let rows = window::rows(overlay, (area.width, area.height));
    let height = window::height((area.width, area.height), rows.len()) + 2;
    let width = overlay.width().min(area.width);
    let at =
        Rect::new(area.x + (area.width - width) / 2, area.y + (area.height.saturating_sub(height)) / 2, width, height);
    let lines: Vec<TextLine> = rows.into_iter().map(|row| TextLine::from(format!("  {row}"))).collect();
    let mut block = Block::bordered().title(overlay.title());
    if let Some(hint) = overlay.hint() {
        block = block.title(TextLine::from(hint).right_aligned());
    }
    if window::more_below(overlay, (area.width, area.height)) {
        block = block.title_bottom(TextLine::from(" ↓ more ─").right_aligned());
    }
    frame.render_widget(Clear, at);
    frame.render_widget(
        Paragraph::new(lines).block(block).scroll((window::scroll(overlay, (area.width, area.height)), 0)),
        at,
    );
}

/// ```text
///  corvo-chat 0.1.0                         title
///
///  Choose a password: at least 10…          what this step says
///  The passwords don't match                notes
/// Signing up…                               busy
/// ────────────────────────────────────────
/// Password: ••••_                           the question
/// ```
fn draw_start(frame: &mut Frame, state: &State, start: &Start, area: Rect) {
    let (w, h) = (area.width, area.height);
    if w < MIN_WIDTH || h < MIN_HEIGHT {
        frame.render_widget(Paragraph::new(TOO_SMALL).wrap(Wrap { trim: true }), area);
        return;
    }
    let title = format!(" {APP} {VERSION}");
    frame.render_widget(Paragraph::new(title).style(Style::new().add_modifier(Modifier::BOLD)), Rect::new(0, 0, w, 1));
    let body = Rect::new(1, 2, w - 2, h - 5);
    let mut lines = start.guidance();
    if !start.notes.is_empty() {
        lines.push(Line { text: String::new(), tone: Tone::Plain });
        lines.extend(start.notes.iter().cloned());
    }
    // Server messages are among the notes: cleaned like anything from elsewhere.
    let rows: Vec<TextLine> = lines
        .iter()
        .flat_map(|line| {
            let style = tone(line.tone);
            line.text
                .split('\n')
                .flat_map(|piece| wrap(&clean(piece), body.width.into()))
                .map(move |row| TextLine::styled(row, style))
                .collect::<Vec<_>>()
        })
        .collect();
    frame.render_widget(Paragraph::new(rows), body);
    draw_flash(frame, state, Rect::new(0, h - 3, w, 1));
    frame.render_widget(Block::new().borders(Borders::TOP).border_style(dim()), Rect::new(0, h - 2, w, 1));
    draw_input(frame, state, Rect::new(0, h - 1, w, 1));
}

/// What the app is busy with, or else the flash.
fn draw_flash(frame: &mut Frame, state: &State, area: Rect) {
    if let Some(busy) = state.busy {
        frame.render_widget(Paragraph::new(busy).style(tone(Tone::Warn)), area);
    } else if let Some(flash) = &state.flash {
        frame.render_widget(Paragraph::new(clean(&flash.line.text)).style(tone(flash.line.tone)), area);
    }
}

fn dim() -> Style {
    Style::new().fg(Color::DarkGray)
}

fn tone(tone: Tone) -> Style {
    match tone {
        Tone::Plain => Style::new(),
        Tone::Dim => dim(),
        Tone::Good => Style::new().fg(Color::Green),
        Tone::Warn => Style::new().fg(Color::Yellow),
        Tone::Bad => Style::new().fg(Color::Red),
        Tone::Own => Style::new().fg(Color::Cyan),
    }
}

/// A name for one row: cleaned, and line breaks become spaces.
fn one_line(text: &str) -> String {
    clean(&text.replace('\n', " "))
}

/// Each tab's title as drawn, padded with a space each side.
fn tab_titles(state: &State) -> Vec<String> {
    state.tabs.iter().map(|tab| format!(" {} ", one_line(&tab.title))).collect()
}

/// The tab drawn at column `x`, for clicks.
pub fn tab_at(state: &State, x: u16) -> Option<usize> {
    let mut start = 0;
    for (i, title) in tab_titles(state).iter().enumerate() {
        let end = start + title.width();
        if (start..end).contains(&usize::from(x)) {
            return Some(i);
        }
        start = end + 1; // the │ between tabs
    }
    None
}

fn draw_tabs(frame: &mut Frame, state: &State, area: Rect) {
    let mut spans = Vec::new();
    for (i, title) in tab_titles(state).into_iter().enumerate() {
        if i > 0 {
            spans.push(Span::styled("│", dim()));
        }
        let style = if i == state.current {
            Style::new().add_modifier(Modifier::REVERSED | Modifier::BOLD)
        } else {
            Style::new()
        };
        spans.push(Span::styled(title, style));
    }
    frame.render_widget(Paragraph::new(TextLine::from(spans)), area);
}

/// A sidebar row, and whose chat a click on it opens.
type SideRow = (TextLine<'static>, Option<Number>);

/// The sidebar's rows: contacts A to Z (a green dot when online), then
/// requests, then the people you asked who haven't accepted yet, by number.
fn sidebar(state: &State) -> Vec<SideRow> {
    let open = state.open_number();
    let unread = |n: Number| -> usize { state.list.iter().filter(|r| r.with == n).map(|r| r.unread).sum() };
    let count = |n: usize| if n > 0 { Span::styled(format!(" ({n})"), tone(Tone::Warn)) } else { Span::raw("") };
    let bold = |n: Number| if open == Some(n) { Style::new().add_modifier(Modifier::BOLD) } else { Style::new() };
    let of = |relation: Relation| -> Vec<Number> {
        let mut people: Vec<(String, String, Number)> = state
            .people
            .iter()
            .filter(|p| p.relation == relation)
            .map(|p| (one_line(&state.label(p.number)).to_lowercase(), p.number.digits(), p.number))
            .collect();
        people.sort_by(|a, b| (&a.0, &a.1).cmp(&(&b.0, &b.1)));
        people.into_iter().map(|(.., n)| n).collect()
    };
    let mut lines = Vec::new();
    for n in of(Relation::Contact) {
        let online = state.person(n).is_some_and(|p| p.online);
        let (dot, dot_style) = if online { ("● ", Style::new().fg(Color::Green)) } else { ("○ ", dim()) };
        let spans =
            vec![Span::styled(dot, dot_style), Span::styled(one_line(&state.label(n)), bold(n)), count(unread(n))];
        lines.push((TextLine::from(spans), Some(n)));
    }
    let group =
        |lines: &mut Vec<SideRow>, head: String, style: Style, people: Vec<Number>, name: &dyn Fn(Number) -> String| {
            if people.is_empty() {
                return;
            }
            if !lines.is_empty() {
                lines.push((TextLine::default(), None));
            }
            lines.push((TextLine::styled(head, style), None));
            for n in people {
                let spans = vec![Span::raw("  "), Span::styled(one_line(&name(n)), bold(n)), count(unread(n))];
                lines.push((TextLine::from(spans), Some(n)));
            }
        };
    let requests = of(Relation::Request);
    let head = format!("Requests ({})", requests.len());
    group(&mut lines, head, tone(Tone::Warn), requests, &|n| state.label(n));
    let asked = of(Relation::Asked);
    let head = format!("Asked ({})", asked.len());
    group(&mut lines, head, dim(), asked, &|n: Number| n.to_string());
    if lines.is_empty() {
        lines = vec![(TextLine::styled("no one yet", dim()), None), (TextLine::styled("/add <number>", dim()), None)];
    }
    lines
}

/// The first sidebar row shown when it's scrolled to `top`, kept in range.
fn sidebar_first(total: usize, height: usize, top: usize) -> usize {
    top.min(total.saturating_sub(height))
}

/// Whether row `i` of `total` is drawn in `height` rows from `first`: the
/// top and bottom rows give way to "↑ N more" and "↓ N more" when there is.
fn sidebar_shows(i: usize, total: usize, height: usize, first: usize) -> bool {
    let above = first > 0;
    let below = first + height < total;
    i >= first + usize::from(above) && i + usize::from(below) < first + height
}

/// The sidebar's rows from the top it's scrolled to, as drawn in `height`
/// rows, each with the person a click on it opens.
fn sidebar_page(state: &State, height: usize) -> Vec<SideRow> {
    let lines = sidebar(state);
    let total = lines.len();
    let first = sidebar_first(total, height, state.sidebar_top);
    let more = |hidden: &[SideRow], arrow: &str| {
        let people: Vec<Number> = hidden.iter().filter_map(|(_, n)| *n).collect();
        let requests =
            people.iter().filter(|n| state.person(**n).is_some_and(|p| p.relation == Relation::Request)).count();
        // Too narrow to say more: hidden requests make it stand out instead.
        let style = if requests > 0 { tone(Tone::Warn) } else { dim() };
        (TextLine::styled(format!("{arrow} {} more", people.len()), style), None)
    };
    let mut page = Vec::new();
    for (i, line) in lines.iter().enumerate().skip(first).take(height) {
        if sidebar_shows(i, total, height, first) {
            page.push(line.clone());
        } else if i == first {
            page.push(more(&lines[..=first], "↑"));
        } else {
            page.push(more(&lines[i..], "↓"));
        }
    }
    page
}

/// Rows in the sidebar: (all of them, how many fit).
fn sidebar_size(state: &State) -> (usize, usize) {
    let height = areas(state.size, input_rows(state)).map_or(0, |a| usize::from(a.sidebar.height));
    (sidebar(state).len(), height)
}

/// Where the sidebar is scrolled after the wheel moves it by `delta` rows.
pub fn sidebar_scrolled(state: &State, delta: isize) -> usize {
    let (total, height) = sidebar_size(state);
    let first = sidebar_first(total, height, state.sidebar_top) as isize;
    sidebar_first(total, height, (first + delta).max(0) as usize)
}

/// Where the sidebar is scrolled so `number`'s row is in view; where it is if it already is.
pub fn sidebar_showing(state: &State, number: Number) -> usize {
    let (total, height) = sidebar_size(state);
    let mut first = sidebar_first(total, height, state.sidebar_top);
    let Some(i) = sidebar(state).iter().position(|(_, n)| *n == Some(number)) else { return first };
    while height > 2 && !sidebar_shows(i, total, height, first) {
        if i < first + 1 { first -= 1 } else { first += 1 }
    }
    first
}

/// Who each sidebar row on screen opens, top to bottom, for clicks.
pub fn sidebar_rows(state: &State) -> Vec<Option<Number>> {
    let (_, height) = sidebar_size(state);
    sidebar_page(state, height).into_iter().map(|(_, n)| n).collect()
}

fn draw_sidebar(frame: &mut Frame, state: &State, area: Rect) {
    let lines: Vec<TextLine> = sidebar_page(state, area.height.into()).into_iter().map(|(line, _)| line).collect();
    frame.render_widget(Paragraph::new(lines).block(Block::new().borders(Borders::RIGHT).border_style(dim())), area);
}

/// Where the completion list goes above `input`: its area (border included),
/// the first match shown and how many show. `None` when it isn't drawn. It
/// sits just above the input, lined up with the word, and shrinks to the rows
/// that fit below the tabs.
pub fn popup_area(state: &State, input: Rect) -> Option<(Rect, usize, usize)> {
    let p = state.popup.as_ref()?;
    let room = usize::from(input.y).saturating_sub(3);
    let rows = p.matches.len().min(POPUP_ROWS).min(room);
    if rows == 0 || p.start > state.editor.before_cursor().len() {
        return None;
    }
    let height = rows as u16 + 2;
    let longest = p.matches.iter().map(|m| one_line(m).width()).max().unwrap_or(0) as u16;
    let width = (longest + 2).min(input.width);
    let before = &state.editor.before_cursor()[..p.start];
    let col = before[before.rfind('\n').map_or(0, |i| i + 1)..].width() as u16;
    let x = (input.x + PROMPT.len() as u16 + col).min(input.x + input.width - width);
    Some((Rect::new(x, input.y - height, width, height), p.selected.saturating_sub(rows - 1), rows))
}

fn draw_popup(frame: &mut Frame, state: &State, input: Rect) {
    let (Some(p), Some((area, first, rows))) = (&state.popup, popup_area(state, input)) else {
        return;
    };
    let lines: Vec<TextLine> = p
        .matches
        .iter()
        .enumerate()
        .skip(first)
        .take(rows)
        .map(|(i, m)| {
            let style = if i == p.selected { Style::new().add_modifier(Modifier::REVERSED) } else { Style::new() };
            TextLine::styled(one_line(m), style)
        })
        .collect();
    frame.render_widget(Clear, area);
    frame.render_widget(Paragraph::new(lines).block(Block::bordered().border_style(dim())), area);
}

fn draw_chat(frame: &mut Frame, state: &State, area: Rect) {
    let (rows, clear_row) = chat_rows(state, area.width);
    let height = usize::from(area.height);
    let total = scroll_total(rows.len(), clear_row, height);
    let top = state.scroll.top(total, height);
    let shown: Vec<TextLine> =
        rows.iter().skip(top).take(height).map(|(text, t)| TextLine::styled(text.as_str(), tone(*t))).collect();
    frame.render_widget(Paragraph::new(shown), area);
    if state.scroll.is_scrolled() && area.height > 0 {
        let more = " ↓ more ";
        let w = (more.width() as u16).min(area.width);
        let at = Rect::new(area.right() - w, area.bottom() - 1, w, 1);
        frame.render_widget(Paragraph::new(more).style(Style::new().add_modifier(Modifier::REVERSED)), at);
    }
}

/// Your own typing, drawn one character per character so the cursor lines up:
/// a masked answer shows `•`, and anything that isn't safe to draw shows as �.
fn shown_char(c: char, masked: bool) -> char {
    if masked {
        MASK
    } else if c.is_control() || is_invisible(c) {
        '\u{FFFD}'
    } else {
        c
    }
}

fn draw_input(frame: &mut Frame, state: &State, area: Rect) {
    let ask = state.ask();
    // A label can hold a name from elsewhere, like the remembered username.
    let label = ask.as_ref().map(|a| clean(&a.label));
    let (prompt, masked) = label.as_deref().map_or((PROMPT, false), |l| (l, ask.as_ref().is_some_and(|a| a.masked)));
    let prompt_w = prompt.width();
    let editor = &state.editor;
    if editor.is_empty() {
        let hint = match ask {
            Some(_) => "",
            None if state.on_corvo() => "/help lists commands",
            None => "type a message",
        };
        let line = TextLine::from(vec![Span::raw(prompt), Span::styled(hint, dim())]);
        frame.render_widget(Paragraph::new(line), area);
        let x = (area.x as usize + prompt_w).min(area.right().saturating_sub(1) as usize);
        frame.set_cursor_position(Position::new(x as u16, area.y));
        return;
    }
    let visible = area.height as usize;
    let (cursor_row, cursor_col) = editor.cursor_pos();
    let first = cursor_row.saturating_sub(visible.saturating_sub(1));
    let room = (area.width as usize).saturating_sub(prompt_w + 1).max(1);
    let indent = " ".repeat(prompt_w);
    let mut lines = Vec::new();
    for (i, row) in editor.rows().into_iter().enumerate().skip(first).take(visible) {
        let chars: Vec<char> = row.chars().map(|c| shown_char(c, masked)).collect();
        let lead = if i == 0 { prompt } else { indent.as_str() };
        let mut skip = 0;
        if i == cursor_row {
            // Keeps the cursor in view on a row wider than the box.
            while chars[skip..cursor_col].iter().map(|c| c.width().unwrap_or(0)).sum::<usize>() > room {
                skip += 1;
            }
            let x: usize = chars[skip..cursor_col].iter().map(|c| c.width().unwrap_or(0)).sum();
            let x = (area.x as usize + prompt_w + x).min(area.right().saturating_sub(1) as usize);
            frame.set_cursor_position(Position::new(x as u16, area.y + (i - first) as u16));
        }
        lines.push(TextLine::from(format!("{lead}{}", chars[skip..].iter().collect::<String>())));
    }
    frame.render_widget(Paragraph::new(lines), area);
}

/// Your name and number, once known, then where the connection is, then
/// as much as fits of what's waiting to go.
fn draw_status(frame: &mut Frame, state: &State, area: Rect) {
    let (word, color) = match state.conn {
        None => ("connecting".to_string(), Color::Yellow),
        Some(ConnState::Online) => ("online".to_string(), Color::Green),
        Some(ConnState::Reconnecting { in_secs }) => (format!("reconnecting in {in_secs}s"), Color::Red),
        Some(ConnState::UpdateRequired) => ("update needed".to_string(), Color::Red),
    };
    let mut lead = String::from(" ");
    for part in [one_line(&state.name), one_line(&state.number)] {
        if !part.is_empty() {
            lead.push_str(&format!("{part} · "));
        }
    }
    let network = format!("● {word}");
    let mut used = lead.width() + network.width();
    let mut spans = vec![Span::raw(lead), Span::styled(network, Style::new().fg(color))];
    for part in status_parts(state) {
        let part = format!(" · {part}");
        if used + part.width() > usize::from(area.width) {
            break;
        }
        used += part.width();
        spans.push(Span::raw(part));
    }
    frame.render_widget(
        Paragraph::new(TextLine::from(spans)).style(Style::new().add_modifier(Modifier::REVERSED)),
        area,
    );
}

/// The status line's parts after the connection, most useful first.
fn status_parts(state: &State) -> Vec<String> {
    let mut parts = Vec::new();
    let waiting = state.waiting();
    if waiting > 0 {
        parts.push(format!("{waiting} waiting"));
    }
    parts
}

/// Draws `state` on a `w`×`h` test terminal, for tests here and elsewhere.
#[cfg(test)]
pub(crate) fn render(state: &State, w: u16, h: u16) -> ratatui::buffer::Buffer {
    let mut terminal = ratatui::Terminal::new(ratatui::backend::TestBackend::new(w, h)).unwrap();
    terminal.draw(|f| draw(f, state)).unwrap();
    terminal.backend().buffer().clone()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ui::update::{Msg, update};
    use ratatui::buffer::Buffer;
    use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

    fn row(buf: &Buffer, y: u16) -> String {
        (0..buf.area.width).map(|x| buf[(x, y)].symbol()).collect()
    }

    fn screen(buf: &Buffer) -> String {
        (0..buf.area.height).map(|y| row(buf, y)).collect::<Vec<_>>().join("\n")
    }

    fn screen_of(state: &State, w: u16, h: u16) -> String {
        screen(&render(state, w, h))
    }

    fn state() -> State {
        State::new("ana".into(), "482 910 5573".into())
    }

    /// Ana's shell with a few lines in the corvo tab, one of them an error.
    fn busy_state() -> State {
        let mut state = state();
        state.say(Tone::Plain, "welcome");
        state.say(Tone::Plain, "two\nlines and 日本語 and \x1b[2J\u{202E}");
        state.say(Tone::Bad, "something went wrong");
        state
    }

    #[test]
    fn clean_removes_escapes_controls_and_direction_marks() {
        assert_eq!(clean("\x1b[31mred\x1b[0m"), "red");
        assert_eq!(clean("a\x1b[2Jb"), "ab");
        assert_eq!(clean("\x1b]0;evil title\x07ok"), "ok");
        assert_eq!(clean("\x1b]52;c;aGk=\x1b\\ok"), "ok");
        assert_eq!(clean("over\rwrite"), "overwrite");
        assert_eq!(clean("\u{202E}txt.exe"), "txt.exe");
        assert_eq!(clean("zero\u{200B}width"), "zerowidth");
        assert_eq!(clean("bell\x07 nul\0 c1\u{9b}"), "bell nul c1");
        assert_eq!(clean("a\tb"), "a b");
        assert_eq!(clean("two\nlines"), "twolines");
    }

    #[test]
    fn clean_leaves_plain_text_and_emoji_alone() {
        for text in ["hello, world!", "नमस्ते", "שלום", "日本語", "👋🏽 👨‍👩‍👧 🏴󠁧󠁢󠁳󠁣󠁴󠁿 ❤️", "a·b ✓✓"]
        {
            assert_eq!(clean(text), text);
        }
    }

    #[test]
    fn wrap_breaks_at_spaces_then_anywhere() {
        assert_eq!(wrap("", 10), vec![""]);
        assert_eq!(wrap("hello world", 20), vec!["hello world"]);
        assert_eq!(wrap("hello there world", 11), vec!["hello there", "world"]);
        assert_eq!(wrap("abcdefghij", 4), vec!["abcd", "efgh", "ij"]);
        assert_eq!(wrap("ab cdefghij", 4), vec!["ab", "cdef", "ghij"]);
        assert_eq!(wrap("日本語です", 4), vec!["日本", "語で", "す"], "wide characters take two columns");
        let long = "x".repeat(300);
        let rows = wrap(&long, 50);
        assert_eq!(rows.len(), 6);
        assert!(rows.iter().all(|r| r.width() <= 50));
    }

    #[test]
    fn the_shell_draws_at_80x24_and_at_the_minimum() {
        let state = state();
        let buf = render(&state, 80, 24);
        assert!(row(&buf, 0).starts_with(" corvo "), "{:?}", row(&buf, 0));
        assert!(row(&buf, 1).starts_with("no one yet"), "{:?}", row(&buf, 1));
        assert!(row(&buf, 21).starts_with("────"));
        assert!(row(&buf, 22).starts_with("> /help lists commands"), "{:?}", row(&buf, 22));
        assert_eq!(row(&buf, 23).trim_end(), " ana · 482 910 5573 · ● connecting");

        let small = render(&state, MIN_WIDTH, MIN_HEIGHT);
        assert!(row(&small, 0).starts_with(" corvo "), "{}", screen(&small));
        assert!(row(&small, 11).starts_with(" ana · 482 910 5573 · ●"), "{}", screen(&small));

        for (w, h) in [(MIN_WIDTH - 1, 24), (80, MIN_HEIGHT - 1)] {
            let buf = render(&state, w, h);
            assert!(row(&buf, 0).starts_with(TOO_SMALL), "{w}×{h}: {}", screen(&buf));
            assert!(!screen(&buf).contains("corvo"), "{w}×{h}: only the notice");
        }
    }

    #[test]
    fn the_status_line_says_where_the_connection_is() {
        let mut state = state();
        let status = |state: &State| row(&render(state, 80, 24), 23).trim_end().to_string();
        state.conn = Some(ConnState::Online);
        assert_eq!(status(&state), " ana · 482 910 5573 · ● online");
        state.conn = Some(ConnState::Reconnecting { in_secs: 4 });
        assert_eq!(status(&state), " ana · 482 910 5573 · ● reconnecting in 4s");
        state.conn = Some(ConnState::UpdateRequired);
        assert_eq!(status(&state), " ana · 482 910 5573 · ● update needed");
        let buf = render(&state, 80, 24);
        let dot = row(&buf, 23).find('●').map(|at| row(&buf, 23)[..at].chars().count()).unwrap();
        assert_eq!(buf[(dot as u16, 23)].fg, Color::Red);
        assert_eq!(status(&State::new(String::new(), String::new())), " ● connecting", "nothing known yet");
    }

    #[test]
    fn the_tips_read_on_80_by_24_under_the_art_and_scroll_on_less() {
        let mut state = state();
        state.overlay = Some(Overlay::start(true));
        let screen = screen_of(&state, 80, 24);
        for part in [
            "┌─ Welcome to corvo ───",
            r"│  | (__ | (_) || |    \ V / | (_) |",
            "│  Chat with friends, end to end encrypted.",
            "│  • Your number is how people find you: /code shows it.",
            "│  • Marks: ○ waiting  · sent  ✓ received  ✓✓ read  ✗ refused  │",
            "│              /help any time · Enter to start",
        ] {
            assert!(screen.contains(part), "{part:?} missing:\n{screen}");
        }
        assert!(!screen.contains("↓ more"), "it all fits on 80×24:\n{screen}");
        let short = screen_of(&state, 40, 12);
        assert!(short.contains("Welcome to corvo") && !short.contains("/help any time"), "{short}");
        let total = window::rows(&Overlay::start(true), (40, 12)).len();
        let end = (total - usize::from(window::height((40, 12), total))) as u16;
        state.overlay = Some(Overlay::Start { first_run: true, ticks_left: window::START_TICKS, scroll: end });
        assert!(screen_of(&state, 40, 12).contains("/help any time"), "scrolled to the end");
    }

    #[test]
    fn the_start_window_fits_40_by_12() {
        let mut state = state();
        state.overlay = Some(Overlay::start(false));
        let screen = screen_of(&state, 40, 12);
        for row in window::ART {
            assert!(screen.contains(row), "{row:?} missing:\n{screen}");
        }
        assert!(screen.contains(&format!("corvo-chat v{}", env!("CARGO_PKG_VERSION"))), "{screen}");
        assert!(!screen.contains("↓ more"), "{screen}");
    }

    #[test]
    fn the_about_window_shows_version_data_folder_and_source() {
        let mut state = state();
        state.overlay = Some(Overlay::About { data_dir: "~/.config/corvo-chat".into(), scroll: 0 });
        let screen = screen_of(&state, 80, 24);
        for part in [
            "┌─ About ",
            " Esc closes ─┐",
            &format!("│  corvo-chat v{}", env!("CARGO_PKG_VERSION")),
            "│  Your data: ~/.config/corvo-chat",
            "│  Source:    github.com/Abinsanth/corvo-chat-client",
            "│  License MIT",
        ] {
            assert!(screen.contains(part), "{part:?} missing:\n{screen}");
        }
        let small = screen_of(&state, 40, 12);
        assert!(small.contains("About") && small.contains("corvo-chat v"), "{small}");
    }

    #[test]
    fn the_help_window_shows_groups_and_keys_without_marks() {
        let mut state = state();
        state.overlay = Some(Overlay::Help { scroll: 0 });
        let screen = screen_of(&state, 100, 50);
        for part in [
            "┌─ Help ─",
            " Esc closes ─┐",
            "│  You        /profile          name, password, devices",
            "│  Friends    /code             your number",
            "│             /add <number>     add a friend",
            "│  Keys       Tab               complete",
        ] {
            assert!(screen.contains(part), "{part:?} missing:\n{screen}");
        }
        assert!(!screen.contains("✓✓") && !screen.contains("Marks"), "{screen}");
    }

    #[test]
    fn a_window_that_does_not_fit_says_there_is_more() {
        let mut state = state();
        state.size = (80, 24);
        state.overlay = Some(Overlay::Help { scroll: 0 });
        let top = screen_of(&state, 80, 24);
        assert!(top.contains("↓ more"), "{top}");
        assert!(!top.contains("what you typed before"), "the end is out of sight on 80×24");
        state.overlay = Some(Overlay::Help { scroll: u16::MAX });
        let end = screen_of(&state, 80, 24);
        assert!(end.contains("what you typed before") && !end.contains("↓ more"), "{end}");
        state.overlay = Some(Overlay::start(false));
        assert!(!screen_of(&state, 80, 24).contains("↓ more"), "the start window fits");
    }

    #[test]
    fn the_layout_at_80_by_24() {
        let mut state = busy_state();
        state.conn = Some(ConnState::Online);
        state.editor.set("hello".into());
        let buf = render(&state, 80, 24);
        assert!(row(&buf, 0).starts_with(" corvo "), "{:?}", row(&buf, 0));
        assert_eq!(row(&buf, 1).chars().nth(19), Some('│'), "the sidebar is 20 columns");
        assert!(row(&buf, 1).contains("welcome"));
        assert!(row(&buf, 20).starts_with("something went wrong"), "the flash");
        assert!(row(&buf, 21).starts_with("────"));
        assert!(row(&buf, 22).starts_with("> hello"));
        assert!(row(&buf, 23).starts_with(" ana · 482 910 5573 · ● online"));
    }

    #[test]
    fn lines_are_drawn_cleaned() {
        let state = busy_state();
        let buf = render(&state, 80, 24);
        let screen: Vec<String> = (0..24).map(|y| row(&buf, y)).collect();
        let at = screen.iter().position(|r| r.contains("lines and") && r.contains('日')).expect("drawn");
        assert!(screen[at - 1].contains("two"), "a line break makes a row");
        assert!(!screen.iter().any(|r| r.contains('\u{202E}') || r.contains('\x1b')));
    }

    #[test]
    fn after_clear_the_tab_is_empty_until_something_new() {
        let mut state = busy_state();
        state.clear_tab();
        let buf = render(&state, 80, 24);
        assert!(!(1..20).any(|y| row(&buf, y).contains("welcome") || row(&buf, y).contains("went wrong")));
        state.say(Tone::Plain, "fresh");
        let buf = render(&state, 80, 24);
        assert!(row(&buf, 1).contains("fresh"), "new lines start at the top");
        let (total, height) = chat_page(&state);
        assert_eq!(total, 4 + height, "what came before is still there to scroll up to");
    }

    #[test]
    fn a_long_input_row_keeps_the_cursor_in_view() {
        let mut state = state();
        state.editor.set(format!("{}END", "x".repeat(200)));
        let buf = render(&state, 80, 24);
        assert!(row(&buf, 22).trim_end().ends_with("END"));
    }

    #[test]
    fn no_panic_at_any_size() {
        let shell = || {
            let mut state = busy_state();
            state.editor.set("one\ntwo\nthree\nfour\nfive\nsix 日本語 \x1b \u{202E}".into());
            state
        };
        let mut scrolled = shell();
        scrolled.scroll.by(-3, 100, 10);
        let mut states = vec![shell(), scrolled];
        for overlay in [Overlay::start(true), Overlay::Help { scroll: 3 }] {
            let mut state = shell();
            state.overlay = Some(overlay);
            states.push(state);
        }
        for state in &states {
            for w in 1..=120 {
                for h in 1..=40 {
                    render(state, w, h);
                }
            }
        }
    }

    #[test]
    fn rows_are_built_once_per_change() {
        let mut state = busy_state();
        state.size = (80, 24);
        let start = BUILDS.with(|b| b.get());
        let built = || BUILDS.with(|b| b.get()) - start;
        render(&state, 80, 24);
        render(&state, 80, 24);
        update(&mut state, Msg::Tick);
        render(&state, 80, 24);
        assert_eq!(built(), 1, "a frame that changes nothing reuses the rows");
        state.say(Tone::Plain, "new");
        update(&mut state, Msg::Key(KeyEvent::new(KeyCode::PageUp, KeyModifiers::NONE)));
        render(&state, 80, 24);
        assert_eq!(built(), 2, "once after a change, scrolling included");
        render(&state, 100, 30);
        assert_eq!(built(), 3, "and again at a new width");
    }

    #[test]
    fn the_completion_list_sits_above_the_input_lined_up_with_the_word() {
        let mut state = state();
        for c in "/bl".chars() {
            update(&mut state, Msg::Key(KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE)));
        }
        update(&mut state, Msg::Key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE)));
        let buf = render(&state, 80, 24);
        let names: Vec<String> = state.popup.as_ref().unwrap().matches.clone();
        assert_eq!(names, ["/block", "/blocked"]);
        let input = areas((80, 24), 1).unwrap().input;
        let first = input.y as usize - names.len() - 1;
        for (i, name) in names.iter().enumerate() {
            let line = row(&buf, (first + i) as u16);
            let column = line.find(name.as_str()).map(|at| line[..at].chars().count());
            assert_eq!(column, Some("> ".len() + 1), "{line:?}");
        }
        let selected = buf[(("> ".len() + 1) as u16, first as u16)].modifier;
        assert!(selected.contains(Modifier::REVERSED));
    }

    #[test]
    fn clicking_an_entry_in_the_completion_list_takes_it() {
        use ratatui::crossterm::event::{MouseButton, MouseEvent, MouseEventKind};
        let mut state = state();
        state.size = (80, 24);
        for c in "/cl".chars() {
            update(&mut state, Msg::Key(KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE)));
        }
        update(&mut state, Msg::Key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE)));
        let names = state.popup.as_ref().unwrap().matches.clone();
        let buf = render(&state, 80, 24);
        let y = (0..24).find(|&y| row(&buf, y).contains(&format!("│{}", names[1]))).unwrap();
        let click = MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: 4,
            row: y,
            modifiers: KeyModifiers::NONE,
        };
        update(&mut state, Msg::Mouse(click));
        assert_eq!(state.editor.text(), format!("{} ", names[1]));
        assert_eq!(state.popup, None);
    }

    #[test]
    fn clicking_the_tab_selects_it() {
        use ratatui::crossterm::event::{MouseButton, MouseEvent, MouseEventKind};
        let mut state = busy_state();
        state.size = (80, 24);
        state.scroll.by(-3, 100, 10);
        let click = |column, row| {
            Msg::Mouse(MouseEvent {
                kind: MouseEventKind::Down(MouseButton::Left),
                column,
                row,
                modifiers: KeyModifiers::NONE,
            })
        };
        assert_eq!(tab_at(&state, 2), Some(0));
        assert_eq!(tab_at(&state, 30), None);
        update(&mut state, click(2, 0));
        assert!(state.on_corvo() && !state.scroll.is_scrolled(), "selecting it goes back to the end");
    }
}
