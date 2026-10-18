//! Tab completion: command names as the first word, and people's names
//! (or numbers, once a digit is typed) after the commands that take one. Pure.

use corvo_chat_proto::chat::Relation;

use super::commands::COMMANDS;
use super::state::{Popup, State};
use super::view::{areas, clean, input_rows};

/// Someone who can be completed: their name as shown (their number if they
/// show none), and their number.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Candidate {
    pub label: String,
    pub number: String,
}

#[cfg(test)]
impl Candidate {
    /// `label` and `number`.
    pub fn new(label: &str, number: &str) -> Candidate {
        Candidate { label: label.into(), number: number.into() }
    }
}

/// What can be completed right now.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Candidates {
    pub contacts: Vec<Candidate>,
    pub requests: Vec<Candidate>,
    pub blocked: Vec<Candidate>,
}

impl Candidates {
    /// The people known here, by how they stand.
    pub fn from_state(state: &State) -> Candidates {
        let of = |relation: Relation| -> Vec<Candidate> {
            state
                .people
                .iter()
                .filter(|p| p.relation == relation)
                .map(|p| {
                    let label = clean(&state.label(p.number)).trim().to_string();
                    Candidate { label, number: p.number.to_string() }
                })
                .filter(|c| !c.label.is_empty())
                .collect()
        };
        Candidates { contacts: of(Relation::Contact), requests: of(Relation::Request), blocked: of(Relation::Blocked) }
    }

    /// Who the command `name` can take.
    fn pool(&self, name: &str) -> Option<Vec<Candidate>> {
        let pool = match name {
            "chat" | "block" | "remove" | "verify" | "report" => [&self.contacts[..], &self.requests[..]].concat(),
            "accept" | "reject" => self.requests.clone(),
            "unblock" => self.blocked.clone(),
            "info" => [&self.contacts[..], &self.requests[..], &self.blocked[..]].concat(),
            _ => return None,
        };
        Some(pool)
    }
}

/// Only the digits of a number as typed.
fn digits(text: &str) -> String {
    text.chars().filter(char::is_ascii_digit).collect()
}

/// The word being completed and what it could become.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Completion {
    /// Byte offset in the input where the word starts.
    pub start: usize,
    /// Full replacements for the word.
    pub matches: Vec<String>,
}

/// Completes what's before the cursor (`before`): a command name as the
/// first word (never after `//`), or after a command that takes a name,
/// everything after it (names can have spaces).
pub fn complete(before: &str, cands: &Candidates) -> Option<Completion> {
    if before.contains('\n') {
        return None;
    }
    let lead = before.len() - before.trim_start().len();
    let command = before[lead..].strip_prefix('/')?;
    if command.starts_with('/') {
        return None;
    }
    let (start, matches): (usize, Vec<String>) = match command.split_once(char::is_whitespace) {
        None => {
            let mut names = COMMANDS.to_vec();
            names.sort_unstable();
            let part = command.to_lowercase();
            (lead, names.into_iter().filter(|n| n.starts_with(&part)).map(|n| format!("/{n}")).collect())
        }
        Some((name, args)) => {
            let pool = cands.pool(&name.to_lowercase())?;
            let word = args.trim_start();
            // A digit first means a number, spaces or not; anything else, a name.
            let mut matches: Vec<String> = if word.starts_with(|c: char| c.is_ascii_digit()) {
                let part = digits(word);
                pool.into_iter().filter(|c| digits(&c.number).starts_with(&part)).map(|c| c.number).collect()
            } else {
                let part = word.to_lowercase();
                pool.into_iter().filter(|c| c.label.to_lowercase().starts_with(&part)).map(|c| c.label).collect()
            };
            matches.sort_by_key(|m| m.to_lowercase());
            matches.dedup();
            (before.len() - word.len(), matches)
        }
    };
    (!matches.is_empty()).then_some(Completion { start, matches })
}

/// The longest start all `matches` share, compared char by char.
pub fn common_prefix(matches: &[String]) -> String {
    let Some(first) = matches.first() else {
        return String::new();
    };
    let mut len = first.chars().count();
    for m in &matches[1..] {
        len = len.min(first.chars().zip(m.chars()).take_while(|(a, b)| a == b).count());
    }
    first.chars().take(len).collect()
}

/// Tab: one match goes straight in, with a space; several fill their shared
/// start and open the list. Not when the screen is too small to show it.
pub fn tab(state: &mut State) {
    if areas(state.size, input_rows(state)).is_none() {
        return;
    }
    let Some(c) = complete(state.editor.before_cursor(), &Candidates::from_state(state)) else {
        return;
    };
    if let [only] = c.matches.as_slice() {
        state.editor.replace_before_cursor(c.start, &format!("{only} "));
        return;
    }
    let prefix = common_prefix(&c.matches);
    if prefix.chars().count() > state.editor.before_cursor()[c.start..].chars().count() {
        state.editor.replace_before_cursor(c.start, &prefix);
    }
    state.popup = Some(Popup { matches: c.matches, selected: 0, start: c.start });
}

/// ↑/↓ (and Tab) in the open list; wraps.
pub fn step(state: &mut State, delta: isize) {
    if let Some(p) = &mut state.popup {
        p.selected = (p.selected as isize + delta).rem_euclid(p.matches.len() as isize) as usize;
    }
}

/// Enter in the open list: puts the choice in the input and closes it.
pub fn accept(state: &mut State) {
    if let Some(p) = state.popup.take() {
        state.editor.replace_before_cursor(p.start, &format!("{} ", p.matches[p.selected]));
    }
}

/// After an edit with the list open: completes again, closing the list when
/// nothing matches.
pub fn refresh(state: &mut State) {
    if state.popup.is_some() {
        let found = complete(state.editor.before_cursor(), &Candidates::from_state(state));
        state.popup = found.map(|c| Popup { matches: c.matches, selected: 0, start: c.start });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ui::state::APP;
    use crate::ui::update::{Msg, update};
    use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

    fn cands() -> Candidates {
        Candidates {
            contacts: vec![
                Candidate::new("ana maria", "123 456 7895"),
                Candidate::new("rahul", "482 910 5572"),
                Candidate::new("rani", "482 911 0003"),
            ],
            requests: vec![Candidate::new("bob", "222 333 4440")],
            blocked: vec![Candidate::new("eve", "555 666 7770")],
        }
    }

    fn matches(before: &str) -> Vec<String> {
        complete(before, &cands()).map(|c| c.matches).unwrap_or_default()
    }

    #[test]
    fn commands_complete_as_the_first_word_only() {
        assert_eq!(matches("/bl"), vec!["/block", "/blocked"]);
        assert_eq!(
            complete("  /q", &cands()),
            Some(Completion { start: 2, matches: vec!["/q".into(), "/quit".into()] })
        );
        assert_eq!(complete("hi /bl", &cands()), None);
        assert_eq!(complete("//bl", &cands()), None);
        assert_eq!(complete("one\n/bl", &cands()), None);
        assert_eq!(complete("hello", &cands()), None);
        assert_eq!(complete("", &cands()), None);
    }

    #[test]
    fn names_complete_after_commands_that_take_one() {
        assert_eq!(
            complete("/chat RA", &cands()),
            Some(Completion { start: 6, matches: vec!["rahul".into(), "rani".into()] })
        );
        assert_eq!(matches("/chat ana m"), vec!["ana maria"], "names can have spaces");
        assert_eq!(matches("/chat "), vec!["ana maria", "bob", "rahul", "rani"], "contacts and requests");
        assert_eq!(matches("/accept "), vec!["bob"], "only requests");
        assert_eq!(matches("/reject "), vec!["bob"]);
        assert_eq!(matches("/unblock "), vec!["eve"], "only the blocked");
        assert_eq!(matches("/info "), vec!["ana maria", "bob", "eve", "rahul", "rani"]);
        assert_eq!(matches("/remove r"), vec!["rahul", "rani"]);
        assert_eq!(matches("/verify r"), vec!["rahul", "rani"]);
        assert_eq!(matches("/block b"), vec!["bob"]);
        assert_eq!(matches("/add r"), Vec::<String>::new(), "/add takes a number");
        assert_eq!(matches("/chat zed"), Vec::<String>::new());
    }

    #[test]
    fn numbers_complete_once_a_digit_is_typed() {
        assert_eq!(matches("/chat 48"), vec!["482 910 5572", "482 911 0003"]);
        assert_eq!(matches("/chat 482 910"), vec!["482 910 5572"], "spaces don't matter");
        assert_eq!(matches("/chat 4829105"), vec!["482 910 5572"]);
        assert_eq!(matches("/unblock 5"), vec!["555 666 7770"]);
        assert_eq!(matches("/accept 4"), Vec::<String>::new(), "only requests");
        assert_eq!(matches("/chat 9"), Vec::<String>::new());
    }

    #[test]
    fn the_people_known_here_complete() {
        use corvo_chat_core::PersonRow;
        use corvo_chat_proto::Number;
        let mut s = state();
        let person = |body, name: Option<&str>, relation| PersonRow {
            number: Number::from_body(body).unwrap(),
            name: name.map(Into::into),
            relation,
            online: false,
            mismatch: false,
            deleted: false,
        };
        s.people = vec![
            person(123_456_789, Some("rahul"), Relation::Contact),
            person(222_333_444, None, Relation::Request),
            person(555_666_777, Some("ev\x1b[2Jil"), Relation::Blocked),
        ];
        let cands = Candidates::from_state(&s);
        let rahul = Number::from_body(123_456_789).unwrap().to_string();
        let asked = Number::from_body(222_333_444).unwrap().to_string();
        assert_eq!(cands.contacts, [Candidate::new("rahul", &rahul)]);
        assert_eq!(cands.requests, [Candidate::new(&asked, &asked)], "no name: the number");
        assert_eq!(cands.blocked[0].label, "evil", "cleaned");
        typed(&mut s, "/chat ra");
        press(&mut s, KeyCode::Tab);
        assert_eq!(s.editor.text(), "/chat rahul ");
    }

    #[test]
    fn common_prefix_is_char_wise() {
        assert_eq!(common_prefix(&["rahul·m4x8".into(), "rahul·pnoh".into()]), "rahul·");
        assert_eq!(common_prefix(&["/q".into()]), "/q");
        assert_eq!(common_prefix(&[]), "");
    }

    fn state() -> State {
        let mut state = State::new("ana".into(), "482 910 5573".into());
        state.title = APP.into();
        state
    }

    fn press(state: &mut State, code: KeyCode) {
        update(state, Msg::Key(KeyEvent::new(code, KeyModifiers::NONE)));
    }

    fn typed(state: &mut State, text: &str) {
        for c in text.chars() {
            press(state, KeyCode::Char(c));
        }
    }

    fn popup(state: &State) -> Option<(Vec<String>, usize)> {
        state.popup.as_ref().map(|Popup { matches, selected, .. }| (matches.clone(), *selected))
    }

    #[test]
    fn tab_with_one_match_completes_it() {
        let mut s = state();
        typed(&mut s, "/cha");
        press(&mut s, KeyCode::Tab);
        assert_eq!(s.editor.text(), "/chat ");
        assert_eq!(s.popup, None);
    }

    #[test]
    fn tab_with_several_fills_their_start_and_opens_the_list() {
        let mut s = state();
        typed(&mut s, "/cl");
        press(&mut s, KeyCode::Tab);
        assert_eq!(s.editor.text(), "/cl");
        assert_eq!(popup(&s), Some((vec!["/clear".into(), "/close".into()], 0)));
        press(&mut s, KeyCode::Tab);
        assert_eq!(popup(&s).unwrap().1, 1, "Tab moves down");
        press(&mut s, KeyCode::Down);
        assert_eq!(popup(&s).unwrap().1, 0, "and wraps");
        press(&mut s, KeyCode::Up);
        assert_eq!(popup(&s).unwrap().1, 1);
        press(&mut s, KeyCode::Enter);
        assert_eq!(s.editor.text(), "/close ", "Enter takes the choice, and sends nothing");
        assert_eq!(s.popup, None);

        s.editor.clear();
        typed(&mut s, "/re");
        press(&mut s, KeyCode::Tab);
        assert_eq!(s.editor.text(), "/re", "nothing more in common");
        typed(&mut s, "q");
        assert_eq!(popup(&s), Some((vec!["/requests".into()], 0)), "typing narrows it");
        press(&mut s, KeyCode::Enter);
        assert_eq!(s.editor.text(), "/requests ");
    }

    #[test]
    fn typing_narrows_the_list_and_other_keys_close_it() {
        let mut s = state();
        typed(&mut s, "/b");
        press(&mut s, KeyCode::Tab);
        assert_eq!(s.editor.text(), "/block", "their shared start");
        assert_eq!(popup(&s), Some((vec!["/block".into(), "/blocked".into()], 0)));
        typed(&mut s, "e");
        assert_eq!(popup(&s), Some((vec!["/blocked".into()], 0)), "typing refreshes the list");
        typed(&mut s, "z");
        assert_eq!(s.popup, None, "nothing matches");
        press(&mut s, KeyCode::Backspace);
        press(&mut s, KeyCode::Tab);
        assert_eq!(s.editor.text(), "/blocked ");

        s.editor.clear();
        typed(&mut s, "/c");
        press(&mut s, KeyCode::Tab);
        press(&mut s, KeyCode::Left);
        assert_eq!(s.popup, None, "moving the cursor closes it");
        press(&mut s, KeyCode::End);
        press(&mut s, KeyCode::Tab);
        press(&mut s, KeyCode::Esc);
        assert_eq!(s.popup, None);
        assert_eq!(s.editor.text(), "/c", "Esc only closes the list");
    }

    #[test]
    fn tab_does_nothing_with_nothing_to_complete() {
        let mut s = state();
        typed(&mut s, "hello");
        press(&mut s, KeyCode::Tab);
        assert_eq!(s.editor.text(), "hello");
        assert_eq!(s.popup, None);
    }
}
