//! The full-screen UI: owns the terminal while it runs and gives it back
//! cleanly on every exit, including panics. One loop merges terminal input, a
//! tick and messages from elsewhere, feeds them to `update` and draws.

pub mod chat;
pub mod commands;
pub mod complete;
pub mod form;
pub mod people;
pub mod profile;
pub mod runner;
pub mod start;
pub mod state;
pub mod table;
pub mod update;
pub mod view;
pub mod window;

use std::collections::VecDeque;
use std::io::{Write, stdout};
use std::ops::ControlFlow;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::JoinHandle;
use std::time::Duration;

use anyhow::Context;
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use ratatui::crossterm::event::{
    self, DisableBracketedPaste, DisableFocusChange, DisableMouseCapture, EnableBracketedPaste, EnableFocusChange,
    EnableMouseCapture, Event as TermEvent, KeyCode, KeyModifiers, KeyboardEnhancementFlags, MouseButton,
    MouseEventKind, PopKeyboardEnhancementFlags, PushKeyboardEnhancementFlags,
};
use ratatui::crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, SetTitle, disable_raw_mode, enable_raw_mode,
    supports_keyboard_enhancement,
};
use ratatui::crossterm::{Command, execute};
use tokio::sync::mpsc;

use self::runner::{Flow, Runner};
use self::state::State;
use self::update::{Effect, Msg, update};

/// How long the input thread waits for a key before checking whether to stop.
const POLL: Duration = Duration::from_millis(100);
const TICK: Duration = Duration::from_millis(250);

/// Disambiguated keys (so Shift/Alt+Enter are distinguishable) with shifted
/// characters reported as typed. Not REPORT_ALL_KEYS: it would send unshifted keys.
const KEYBOARD_FLAGS: KeyboardEnhancementFlags =
    KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES.union(KeyboardEnhancementFlags::REPORT_ALTERNATE_KEYS);

/// xterm's title stack: save the window title on entry and put it back on exit.
/// Terminals without one ignore both.
const SAVE_TITLE: &str = "\x1b[22;0t";
const RESTORE_TITLE: &str = "\x1b[23;0t";

/// What puts the terminal back the way the shell expects, as escape codes.
/// Written as plain bytes so it's the same on every system and can be tested.
fn restore_sequence() -> String {
    let mut out = String::new();
    let commands: [&dyn Fn(&mut String) -> std::fmt::Result; 6] = [
        &|s| PopKeyboardEnhancementFlags.write_ansi(s),
        &|s| DisableBracketedPaste.write_ansi(s),
        &|s| DisableMouseCapture.write_ansi(s),
        &|s| DisableFocusChange.write_ansi(s),
        &|s| LeaveAlternateScreen.write_ansi(s),
        // The title we set mustn't outlive us where there's no title stack.
        &|s| SetTitle("").write_ansi(s),
    ];
    for write in commands {
        let _ = write(&mut out);
    }
    out.push_str(RESTORE_TITLE);
    out
}

/// Best-effort and safe to repeat.
fn restore_terminal() {
    let mut out = stdout();
    let _ = out.write_all(restore_sequence().as_bytes()).and_then(|()| out.flush());
    let _ = disable_raw_mode();
}

/// Restores the terminal before a panic message prints, so it stays readable.
/// Only for a panic on the UI thread: one in a tokio task is caught there and
/// the UI keeps running.
pub fn install_panic_hook() {
    let previous = std::panic::take_hook();
    let ui_thread = std::thread::current().id();
    std::panic::set_hook(Box::new(move |info| {
        if std::thread::current().id() == ui_thread {
            restore_terminal();
        }
        previous(info);
    }));
}

/// Owns the terminal setup. Dropping it, also while a panic unwinds, writes
/// the restore sequence and leaves raw mode.
struct Guard<W: Write> {
    out: W,
    raw: bool,
}

impl Guard<std::io::Stdout> {
    fn enter() -> std::io::Result<Self> {
        enable_raw_mode()?;
        let mut guard = Guard { out: stdout(), raw: true };
        guard.out.write_all(SAVE_TITLE.as_bytes())?;
        execute!(guard.out, EnterAlternateScreen, EnableMouseCapture, EnableBracketedPaste, EnableFocusChange)?;
        if supports_keyboard_enhancement().unwrap_or(false) {
            execute!(guard.out, PushKeyboardEnhancementFlags(KEYBOARD_FLAGS))?;
        }
        Ok(guard)
    }
}

impl<W: Write> Drop for Guard<W> {
    fn drop(&mut self) {
        let _ = self.out.write_all(restore_sequence().as_bytes()).and_then(|()| self.out.flush());
        if self.raw {
            let _ = disable_raw_mode();
        }
    }
}

/// Reads terminal events on its own thread. Stopped and joined before the
/// terminal is given back, so nothing reads it afterwards.
struct InputThread {
    stop: Arc<AtomicBool>,
    handle: Option<JoinHandle<()>>,
}

impl InputThread {
    fn start(tx: mpsc::UnboundedSender<TermEvent>) -> Self {
        let stop = Arc::new(AtomicBool::new(false));
        let flag = stop.clone();
        let handle = std::thread::spawn(move || {
            while !flag.load(Ordering::SeqCst) {
                match event::poll(POLL) {
                    Ok(true) => {
                        let Ok(ev) = event::read() else { return };
                        if tx.send(ev).is_err() {
                            return;
                        }
                    }
                    Ok(false) => {}
                    Err(_) => return,
                }
            }
        });
        InputThread { stop, handle: Some(handle) }
    }
}

impl Drop for InputThread {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

/// The terminal events `update` cares about. Mouse moves are dropped so they
/// don't cause redraws.
fn to_msg(ev: TermEvent) -> Option<Msg> {
    match ev {
        TermEvent::Key(key) => Some(Msg::Key(key)),
        TermEvent::Paste(text) => Some(Msg::Paste(text)),
        TermEvent::Mouse(m)
            if matches!(
                m.kind,
                MouseEventKind::ScrollUp | MouseEventKind::ScrollDown | MouseEventKind::Down(MouseButton::Left)
            ) =>
        {
            Some(Msg::Mouse(m))
        }
        TermEvent::Resize(w, h) => Some(Msg::Resize(w, h)),
        TermEvent::FocusGained => Some(Msg::Focus(true)),
        TermEvent::FocusLost => Some(Msg::Focus(false)),
        TermEvent::Mouse(_) => None,
    }
}

/// What to keep of the input that arrived during a slow step, like signing
/// in: only resizes and Ctrl-C. Anything typed ahead would land on the next
/// screen, maybe unmasked, so a password typed twice would show.
fn typed_ahead(rx: &mut mpsc::UnboundedReceiver<TermEvent>) -> Vec<Msg> {
    let mut kept = Vec::new();
    while let Ok(ev) = rx.try_recv() {
        match ev {
            TermEvent::Resize(..) => kept.extend(to_msg(ev)),
            TermEvent::Key(key) if key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL) => {
                kept.push(Msg::Key(key))
            }
            _ => {}
        }
    }
    kept
}

/// How the UI ended, for what to print once the terminal is back.
#[derive(Debug, PartialEq, Eq)]
pub enum Ending {
    /// `/quit` or Ctrl-C.
    Quit,
    /// It had to stop: why.
    Exit(String),
    /// The account was deleted: what was done, for after the goodbye.
    Leave(Vec<String>),
}

/// Adds `new` to the work queued. Quitting goes first: Ctrl-C during slow
/// work stops it before the next step.
fn enqueue(queue: &mut VecDeque<Effect>, new: Vec<Effect>) {
    for effect in new {
        if effect == Effect::Quit {
            queue.push_front(effect);
        } else {
            queue.push_back(effect);
        }
    }
}

/// What the loop does after `flow`: go on, maybe with a message, or end.
fn after(flow: Flow) -> ControlFlow<Ending, Option<Msg>> {
    match flow {
        Flow::Next(msg) => ControlFlow::Continue(msg),
        Flow::Quit => ControlFlow::Break(Ending::Quit),
        Flow::Exit(why) => ControlFlow::Break(Ending::Exit(why)),
        Flow::Leave(notes) => ControlFlow::Break(Ending::Leave(notes)),
    }
}

/// Runs the UI until it ends.
pub async fn run<W: Write>(mut state: State, mut runner: Runner<W>) -> anyhow::Result<Ending> {
    let result = interact(&mut state, &mut runner).await;
    // After the terminal is back, so leaving looks instant.
    runner.shutdown().await;
    result
}

/// The loop: draw, then carry out one waiting effect or wait for the next
/// message: a key, a reply from the core, an update or a tick. Drawing
/// before each effect shows what's slow, like "signing in…". Calls to the
/// core don't wait here: their replies come back as messages.
async fn interact<W: Write>(state: &mut State, runner: &mut Runner<W>) -> anyhow::Result<Ending> {
    // Declared in this order so they drop in reverse: the input thread stops
    // before the guard gives the terminal back.
    let _guard = Guard::enter().context("couldn't set up the terminal")?;
    let mut terminal = Terminal::new(CrosstermBackend::new(stdout()))?;
    let (term_tx, mut term_rx) = mpsc::unbounded_channel();
    let _input = InputThread::start(term_tx);
    let size = terminal.size()?;
    let mut effects = VecDeque::new();
    enqueue(&mut effects, update(state, Msg::Resize(size.width, size.height)));
    runner.check_for_update();
    let mut tick = tokio::time::interval(TICK);
    loop {
        terminal.draw(|frame| view::draw(frame, state))?;
        // Whether it was a slow step, to drop what was typed during it.
        let (msg, slow) = match effects.pop_front() {
            Some(effect) => {
                // A call to the core runs on its own; only the start screens' steps hold the loop.
                let slow = effect.busy().is_some() && !matches!(effect, Effect::Call(_));
                match after(runner.apply(effect).await?) {
                    ControlFlow::Continue(msg) => (msg, slow),
                    ControlFlow::Break(ending) => return Ok(ending),
                }
            }
            None => (
                tokio::select! {
                ev = term_rx.recv() => match ev {
                    Some(ev) => to_msg(ev),
                    None => anyhow::bail!("the terminal stopped sending input"),
                },
                msg = runner.next() => Some(msg),
                _ = tick.tick() => Some(Msg::Tick),
                },
                false,
            ),
        };
        if let Some(msg) = msg {
            enqueue(&mut effects, update(state, msg));
        }
        if slow {
            for msg in typed_ahead(&mut term_rx) {
                enqueue(&mut effects, update(state, msg));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    /// A writer the test can read back after the guard is gone.
    #[derive(Clone, Default)]
    struct Shared(Arc<Mutex<Vec<u8>>>);

    impl Write for Shared {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(buf);
            Ok(buf.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn the_terminal_is_restored_on_drop() {
        let out = Shared::default();
        drop(Guard { out: out.clone(), raw: false });
        assert_eq!(String::from_utf8(out.0.lock().unwrap().clone()).unwrap(), restore_sequence());

        // And while a panic unwinds.
        let out = Shared::default();
        let written = out.clone();
        let result = std::panic::catch_unwind(move || {
            let _guard = Guard { out, raw: false };
            panic!("something broke while drawing");
        });
        assert!(result.is_err());
        let written = String::from_utf8(written.0.lock().unwrap().clone()).unwrap();
        assert_eq!(written, restore_sequence());
        for part in ["\x1b[?1049l", "\x1b[?2004l", "\x1b[?1000l", "\x1b[?1004l", RESTORE_TITLE] {
            assert!(written.contains(part), "{part:?} is written");
        }
    }

    #[test]
    fn quitting_ends_with_quit_and_a_fatal_error_with_exit() {
        assert!(matches!(after(Flow::Quit), ControlFlow::Break(Ending::Quit)));
        let exit = after(Flow::Exit("open elsewhere".into()));
        assert!(matches!(exit, ControlFlow::Break(Ending::Exit(why)) if why == "open elsewhere"));
        assert!(matches!(after(Flow::Next(Some(Msg::Tick))), ControlFlow::Continue(Some(Msg::Tick))));
        let leave = after(Flow::Leave(vec!["deleted".into()]));
        assert!(matches!(leave, ControlFlow::Break(Ending::Leave(notes)) if notes == ["deleted"]));
    }

    #[test]
    fn keyboard_flags_report_shifted_characters() {
        use KeyboardEnhancementFlags as F;
        assert!(KEYBOARD_FLAGS.contains(F::DISAMBIGUATE_ESCAPE_CODES));
        assert!(KEYBOARD_FLAGS.contains(F::REPORT_ALTERNATE_KEYS));
        assert!(!KEYBOARD_FLAGS.contains(F::REPORT_ALL_KEYS_AS_ESCAPE_CODES));
    }

    #[test]
    fn what_was_typed_during_a_slow_step_is_dropped_but_resizes_and_ctrl_c_are_kept() {
        use ratatui::crossterm::event::KeyEvent;
        let (tx, mut rx) = mpsc::unbounded_channel();
        let key = |code, modifiers| TermEvent::Key(KeyEvent::new(code, modifiers));
        for c in "hunter2".chars() {
            tx.send(key(KeyCode::Char(c), KeyModifiers::NONE)).unwrap();
        }
        tx.send(key(KeyCode::Enter, KeyModifiers::NONE)).unwrap();
        tx.send(TermEvent::Paste("hunter2".into())).unwrap();
        tx.send(TermEvent::Resize(90, 30)).unwrap();
        tx.send(key(KeyCode::Char('c'), KeyModifiers::CONTROL)).unwrap();
        let kept = typed_ahead(&mut rx);
        assert!(matches!(kept[..], [Msg::Resize(90, 30), Msg::Key(k)] if k.code == KeyCode::Char('c')), "{kept:?}");
        assert!(rx.try_recv().is_err(), "nothing is left to read");
    }

    #[test]
    fn quit_goes_before_work_already_queued() {
        let mut queue: VecDeque<Effect> = VecDeque::from([Effect::SetTitle("a".into())]);
        enqueue(&mut queue, vec![Effect::SetTitle("b".into()), Effect::Quit]);
        assert_eq!(Vec::from(queue), [Effect::Quit, Effect::SetTitle("a".into()), Effect::SetTitle("b".into())]);
    }

    #[test]
    fn mouse_moves_are_dropped_but_the_wheel_is_kept() {
        use ratatui::crossterm::event::{KeyModifiers, MouseEvent};
        let mouse = |kind| TermEvent::Mouse(MouseEvent { kind, column: 0, row: 0, modifiers: KeyModifiers::NONE });
        assert!(to_msg(mouse(MouseEventKind::Moved)).is_none());
        assert!(matches!(to_msg(mouse(MouseEventKind::ScrollUp)), Some(Msg::Mouse(_))));
        assert!(matches!(to_msg(TermEvent::Resize(10, 5)), Some(Msg::Resize(10, 5))));
        assert!(matches!(to_msg(TermEvent::FocusLost), Some(Msg::Focus(false))));
        let click = MouseEventKind::Down(ratatui::crossterm::event::MouseButton::Left);
        assert!(matches!(to_msg(mouse(click)), Some(Msg::Mouse(_))));
    }
}
