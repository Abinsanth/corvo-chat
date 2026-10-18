//! Windows with fields, opened from the Profile tab: change your display
//! name, change your password, make a new recovery code, see your devices,
//! delete your account. What's typed goes into the window, never the input
//! box or its history, and a window's secrets are wiped when it closes
//! (`state.overlay = None` drops them).

use std::fmt;

use chrono::{Local, TimeZone};
use corvo_chat_core::native::{Command, Reply};
use corvo_chat_core::{ConnState, Error, check_password};
use corvo_chat_proto::frames::{DeviceInfo, DeviceKind};
use corvo_chat_proto::names::{check_device_name, check_display_name};
use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};
use zeroize::Zeroizing;

use super::start::{is_last_group, say};
use super::state::{Line, Secret, State, Tone};
use super::table::{cut, table};
use super::update::{Effect, one_line_answer};
use super::view::{clean, wrap};
use super::window::Overlay;

/// The most a field holds, in bytes. Its buffer is this big from the start,
/// so typing never moves it and leaves a copy behind.
const MAX_FIELD: usize = 1024;

/// What Delete account says first.
pub const DELETE_WARNING: &str =
    "This deletes your account, your number and your side of every chat. It can't be undone.";
/// What has to be typed to go on.
const DELETE_WORD: &str = "delete";
/// What Change password says once it's done.
pub const PASSWORD_CHANGED: &str = "Password changed. Your other devices were signed out.";
/// What's printed after the goodbye once the account is deleted.
pub const ACCOUNT_DELETED: &str = "Your account was deleted.";
/// What a wrong password is called here: the core's text is for signing in.
const WRONG_PASSWORD: &str = "Wrong password";
/// The widest a device's name gets in the table.
const DEVICE_NAME: usize = 16;

#[derive(Clone, PartialEq, Eq)]
pub struct Field {
    pub label: &'static str,
    pub value: Zeroizing<String>,
    /// Each character shows as `•`.
    pub masked: bool,
    /// Shown, not typed in.
    pub read_only: bool,
}

impl fmt::Debug for Field {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Field").field("label", &self.label).field("masked", &self.masked).finish_non_exhaustive()
    }
}

impl Field {
    fn typed(label: &'static str, masked: bool) -> Field {
        Field { label, value: Zeroizing::new(String::with_capacity(MAX_FIELD)), masked, read_only: false }
    }

    fn shown(label: &'static str, value: &str) -> Field {
        Field { label, value: Zeroizing::new(value.to_string()), masked: false, read_only: true }
    }

    /// A field to type in that starts with `value`.
    fn filled(label: &'static str, value: &str) -> Field {
        let mut field = Field::typed(label, false);
        for c in value.chars() {
            field.push(c);
        }
        field
    }

    /// What was typed, moved out (not copied), leaving the field empty.
    fn take(&mut self) -> Secret {
        let empty = String::with_capacity(MAX_FIELD);
        Secret::new(std::mem::replace(&mut *self.value, empty))
    }

    fn push(&mut self, c: char) {
        if !self.read_only && self.value.len() + c.len_utf8() <= MAX_FIELD {
            self.value.push(c);
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FormKind {
    Name,
    Password,
    /// The password; then the new code; then its last group, typed back.
    RecoveryCode,
    /// The devices, one chosen; `S` signs another out, `R` renames this one.
    Devices,
    DeleteAccount {
        step: DeleteStep,
    },
}

/// Delete account's steps, in order.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeleteStep {
    /// What goes, and that it can't be undone.
    Warn,
    Password,
    /// Type `delete`.
    Confirm,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Form {
    pub kind: FormKind,
    pub fields: Vec<Field>,
    /// The field being typed in.
    pub focus: usize,
    /// What happened: a mistake, a question, or the result.
    pub note: Option<Line>,
    /// Waiting for the server: keys do nothing, and Esc only where it's safe.
    pub waiting: bool,
    /// The result is shown: any key closes it.
    pub done: bool,
    /// Delete account: the password from its second step, until `delete` is typed.
    held: Option<Secret>,
    /// New recovery code: the code, until its last group is typed back.
    code: Option<Secret>,
    /// Devices: as last listed, oldest sign-in first.
    pub devices: Vec<DeviceInfo>,
    /// Devices: the row marked `›`.
    pub selected: usize,
    /// Devices: the list is the one saved here, since there's no connection.
    pub offline: bool,
    /// Devices: the list came at least once.
    loaded: bool,
    /// Devices: the one "Sign out <name>? y/n" is about.
    asking: Option<DeviceInfo>,
}

impl Form {
    fn new(kind: FormKind, fields: Vec<Field>) -> Form {
        let focus = fields.iter().position(|f| !f.read_only).unwrap_or(0);
        Form {
            kind,
            fields,
            focus,
            note: None,
            waiting: false,
            done: false,
            held: None,
            code: None,
            devices: Vec::new(),
            selected: 0,
            offline: false,
            loaded: false,
            asking: None,
        }
    }

    pub fn name(now: &str) -> Form {
        Form::new(FormKind::Name, vec![Field::shown("Now", now), Field::typed("New", false)])
    }

    pub fn password() -> Form {
        let fields = vec![
            Field::typed("Current password", true),
            Field::typed("New password", true),
            Field::typed("Again", true),
        ];
        Form::new(FormKind::Password, fields)
    }

    pub fn recovery_code() -> Form {
        Form::new(FormKind::RecoveryCode, vec![Field::typed("Password", true)])
    }

    /// Empty until the list comes (ask for it with `Command::Devices`).
    pub fn devices() -> Form {
        let mut form = Form::new(FormKind::Devices, Vec::new());
        form.waiting = true;
        form.say(Tone::Dim, "Asking for your devices…");
        form
    }

    /// A warning first, with no fields; each step brings its own.
    pub fn delete_account() -> Form {
        Form::new(FormKind::DeleteAccount { step: DeleteStep::Warn }, Vec::new())
    }

    pub fn title(&self) -> String {
        match self.kind {
            FormKind::Name => "─ Display name ".into(),
            FormKind::Password => "─ Change password ".into(),
            FormKind::RecoveryCode => "─ New recovery code ".into(),
            FormKind::Devices if self.offline => "─ Devices (offline) ".into(),
            FormKind::Devices => "─ Devices ".into(),
            FormKind::DeleteAccount { .. } => "─ Delete account ".into(),
        }
    }

    /// The new recovery code is on screen, waiting for Enter.
    fn shows_code(&self) -> bool {
        self.code.is_some() && self.fields.is_empty()
    }

    /// Whether Esc closes the window now. Not while a password change, a new
    /// code or a deletion is with the server (what came of it would be lost),
    /// nor while a new code waits to be written down.
    fn esc_closes(&self) -> bool {
        match self.kind {
            _ if self.done => true,
            FormKind::Name => true,
            FormKind::Devices => self.asking.is_none() && self.fields.is_empty(),
            FormKind::RecoveryCode => !self.waiting && self.code.is_none(),
            FormKind::Password | FormKind::DeleteAccount { .. } => !self.waiting,
        }
    }

    /// What the top border says at its right end.
    pub fn border_hint(&self) -> Option<&'static str> {
        self.esc_closes().then_some(" Esc closes ─")
    }

    /// The last row: what the keys do now.
    fn hint(&self) -> &'static str {
        match self.kind {
            _ if self.done => "any key closes",
            FormKind::Devices if !self.loaded => "Esc closes",
            _ if self.waiting => "waiting for the server…",
            FormKind::Name => "Enter saves · Esc cancels",
            FormKind::Password => "Tab next · Enter changes it · Esc cancels",
            FormKind::RecoveryCode if self.code.is_none() => "Enter makes a new code · Esc cancels",
            FormKind::RecoveryCode if self.fields.is_empty() => "Enter once it's written down",
            FormKind::RecoveryCode => "Enter checks it · Esc shows the code again",
            FormKind::Devices if self.asking.is_some() => "y or n",
            FormKind::Devices if !self.fields.is_empty() => "Enter renames it · Esc cancels",
            FormKind::Devices => "↑/↓ choose · S sign out · R rename · Esc closes",
            FormKind::DeleteAccount { .. } => "Enter goes on · Esc cancels",
        }
    }

    /// What the window says above its fields.
    fn text(&self) -> Vec<(&'static str, Tone)> {
        match self.kind {
            FormKind::RecoveryCode if self.code.is_none() => {
                vec![("A new code replaces your old one, which stops working then.", Tone::Plain)]
            }
            FormKind::RecoveryCode if self.fields.is_empty() => {
                vec![("Your old recovery code no longer works. Write this one down.", Tone::Plain)]
            }
            FormKind::RecoveryCode => vec![("Type the last group of your recovery code", Tone::Plain)],
            FormKind::Devices if !self.fields.is_empty() => vec![("A new name for this device:", Tone::Plain)],
            FormKind::DeleteAccount { step: DeleteStep::Warn } => vec![(DELETE_WARNING, Tone::Warn)],
            FormKind::DeleteAccount { step: DeleteStep::Password } => {
                vec![("Your password, to be sure it's you:", Tone::Plain)]
            }
            FormKind::DeleteAccount { step: DeleteStep::Confirm } => {
                vec![("Type delete to delete your account:", Tone::Plain)]
            }
            _ => Vec::new(),
        }
    }

    fn say(&mut self, tone: Tone, text: impl Into<String>) {
        self.note = Some(Line { text: text.into(), tone });
    }

    fn step_focus(&mut self, delta: isize) {
        let editable: Vec<usize> = (0..self.fields.len()).filter(|&i| !self.fields[i].read_only).collect();
        if let Some(at) = editable.iter().position(|&i| i == self.focus) {
            let to = (at as isize + delta).rem_euclid(editable.len() as isize) as usize;
            self.focus = editable[to];
        }
    }

    /// Esc: whatever it undoes here. `true` if the window closes.
    fn escape(&mut self) -> bool {
        if self.esc_closes() {
            return true;
        }
        match self.kind {
            // Back to the code, to look at it again.
            FormKind::RecoveryCode if self.code.is_some() && !self.fields.is_empty() => {
                self.fields.clear();
                self.note = None;
            }
            FormKind::Devices if !self.waiting => {
                self.asking = None;
                self.fields.clear();
                self.note = None;
            }
            _ => {}
        }
        false
    }

    /// Enter: checks what was typed and asks the core to do it.
    fn submit(&mut self) -> Vec<Effect> {
        match self.kind {
            FormKind::Name => match check_display_name(&self.fields[1].value) {
                Ok(name) => {
                    self.waiting = true;
                    self.say(Tone::Dim, "Saving…");
                    vec![Effect::Call(Command::SetDisplayName { name })]
                }
                Err(_) => {
                    self.say(Tone::Bad, "A display name is 1 to 32 characters.");
                    Vec::new()
                }
            },
            FormKind::Password => self.change_password(),
            FormKind::RecoveryCode if self.code.is_none() => {
                if self.fields[0].value.is_empty() {
                    self.say(Tone::Bad, "Type your password");
                    return Vec::new();
                }
                let password = self.fields[0].take().into_zeroizing();
                self.waiting = true;
                self.say(Tone::Dim, "Making a new code…");
                vec![Effect::Call(Command::NewRecoveryCode { password })]
            }
            FormKind::RecoveryCode => {
                let typed = self.fields[0].take();
                self.fields.clear();
                match &self.code {
                    Some(code) if is_last_group(typed.expose(), code.expose()) => {
                        self.code = None;
                        self.done = true;
                        self.say(Tone::Good, "Done: your new recovery code works from now on.");
                    }
                    _ => self.say(Tone::Bad, "That isn't the last group. Look at the code again."),
                }
                Vec::new()
            }
            FormKind::Devices => match check_device_name(&self.fields[0].value) {
                Ok(name) => {
                    self.waiting = true;
                    self.say(Tone::Dim, "Renaming it…");
                    vec![Effect::Call(Command::RenameDevice { name })]
                }
                Err(_) => {
                    self.say(Tone::Bad, "A device name is 1 to 64 characters.");
                    Vec::new()
                }
            },
            FormKind::DeleteAccount { step } => self.delete_step(step),
        }
    }

    /// Change password's Enter: all three typed, the new ones the same and
    /// good enough, before anything goes to the server.
    fn change_password(&mut self) -> Vec<Effect> {
        if self.fields.iter().any(|f| f.value.is_empty()) {
            self.say(Tone::Bad, "Type all three");
            return Vec::new();
        }
        if self.fields[1].value != self.fields[2].value {
            self.fields[2].take();
            self.focus = 2;
            self.say(Tone::Bad, "The passwords don't match");
            return Vec::new();
        }
        if let Err(e) = check_password(&self.fields[1].value) {
            self.fields[1].take();
            self.fields[2].take();
            self.focus = 1;
            self.say(Tone::Bad, say(&e));
            return Vec::new();
        }
        let old = self.fields[0].take().into_zeroizing();
        let new = self.fields[1].take().into_zeroizing();
        self.fields[2].take();
        self.waiting = true;
        self.say(Tone::Dim, "Changing it…");
        vec![Effect::Call(Command::ChangePassword { old, new })]
    }

    /// Enter in the Delete account window: the next step, once this one's done.
    fn delete_step(&mut self, step: DeleteStep) -> Vec<Effect> {
        match step {
            DeleteStep::Warn => self.go_to(DeleteStep::Password, Field::typed("Password", true)),
            DeleteStep::Password if self.fields[0].value.is_empty() => self.say(Tone::Bad, "Type your password"),
            DeleteStep::Password => {
                self.held = Some(self.fields[0].take());
                self.go_to(DeleteStep::Confirm, Field::typed("Type", false));
            }
            DeleteStep::Confirm if self.fields[0].value.as_str() == DELETE_WORD => {
                let Some(password) = self.held.take() else {
                    self.go_to(DeleteStep::Password, Field::typed("Password", true));
                    return Vec::new();
                };
                self.fields[0].take();
                self.waiting = true;
                self.say(Tone::Dim, "Deleting your account…");
                return vec![Effect::Call(Command::DeleteAccount { password: password.into_zeroizing() })];
            }
            DeleteStep::Confirm => {
                self.fields[0].take();
                self.say(Tone::Bad, "That isn't delete: type delete, or Esc to keep your account");
            }
        }
        Vec::new()
    }

    /// On to Delete account's `step`, with its field.
    fn go_to(&mut self, step: DeleteStep, field: Field) {
        self.kind = FormKind::DeleteAccount { step };
        self.fields = vec![field];
        self.focus = 0;
        self.note = None;
    }

    /// A key in the Devices list (not while renaming).
    fn devices_key(&mut self, code: KeyCode, modified: bool) -> Vec<Effect> {
        let letter = |want: char| !modified && matches!(code, KeyCode::Char(c) if c.eq_ignore_ascii_case(&want));
        if let Some(device) = &self.asking {
            if letter('y') {
                let id = device.id.0;
                self.waiting = true;
                self.say(Tone::Dim, "Signing it out…");
                return vec![Effect::Call(Command::SignOutDevice { id })];
            }
            if letter('n') {
                self.asking = None;
                self.note = None;
            }
            return Vec::new();
        }
        let chosen = self.devices.get(self.selected).cloned();
        match code {
            KeyCode::Up => self.selected = self.selected.saturating_sub(1),
            KeyCode::Down => self.selected = (self.selected + 1).min(self.devices.len().saturating_sub(1)),
            _ if letter('s') => match chosen {
                Some(device) if device.this_device => {
                    self.say(Tone::Bad, "That's this device: sign it out from another one.")
                }
                Some(device) => {
                    self.say(Tone::Plain, format!("Sign out {}? y/n", device_name(&device)));
                    self.asking = Some(device);
                }
                None => {}
            },
            _ if letter('r') => match chosen {
                Some(device) if device.this_device => {
                    self.fields = vec![Field::filled("Name", &device.name)];
                    self.focus = 0;
                    self.note = None;
                }
                Some(_) => self.say(Tone::Bad, "Only this device can be renamed here."),
                None => {}
            },
            _ => {}
        }
        Vec::new()
    }

    /// A new list of devices, keeping the same one chosen where it can.
    fn set_devices(&mut self, devices: Vec<DeviceInfo>) {
        let chosen = self.devices.get(self.selected).map(|d| d.id.0);
        self.selected = chosen.and_then(|id| devices.iter().position(|d| d.id.0 == id)).unwrap_or(0);
        self.selected = self.selected.min(devices.len().saturating_sub(1));
        self.devices = devices;
        self.loaded = true;
    }
}

/// A key while a form is open. Ctrl-C never gets here: it quits.
pub fn key(state: &mut State, key: KeyEvent) -> Vec<Effect> {
    let Some(Overlay::Form(form)) = &mut state.overlay else { return Vec::new() };
    if key.kind == KeyEventKind::Release {
        return Vec::new();
    }
    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
    let alt = key.modifiers.contains(KeyModifiers::ALT);
    if form.done {
        state.overlay = None;
        return Vec::new();
    }
    if key.code == KeyCode::Esc {
        if form.escape() {
            state.overlay = None;
        }
        return Vec::new();
    }
    if form.waiting {
        return Vec::new();
    }
    if form.kind == FormKind::Devices && form.fields.is_empty() {
        return form.devices_key(key.code, ctrl || alt);
    }
    if form.shows_code() {
        if key.code == KeyCode::Enter {
            form.fields = vec![Field::typed("Last group", true)];
            form.focus = 0;
            form.note = None;
        }
        return Vec::new();
    }
    match key.code {
        KeyCode::Enter => return form.submit(),
        KeyCode::Tab | KeyCode::Down => form.step_focus(1),
        KeyCode::BackTab | KeyCode::Up => form.step_focus(-1),
        KeyCode::Backspace => {
            if let Some(field) = form.fields.get_mut(form.focus)
                && !field.read_only
            {
                field.value.pop();
            }
        }
        // Windows reports AltGr chords, like @ on a German keyboard, as Ctrl+Alt.
        KeyCode::Char(c) if !(ctrl ^ alt) => {
            if let Some(field) = form.fields.get_mut(form.focus) {
                field.push(c);
            }
        }
        _ => {}
    }
    Vec::new()
}

/// A paste while a form is open goes into the field being typed in, on one
/// line and without a line ending, as an answer at Unlock would: a password
/// pasted here is the one that unlocks. It's wiped after.
pub fn paste(state: &mut State, text: String) {
    let text = one_line_answer(text);
    let Some(Overlay::Form(form)) = &mut state.overlay else { return };
    if form.waiting || form.done {
        return;
    }
    let Some(field) = form.fields.get_mut(form.focus) else { return };
    for c in text.chars() {
        field.push(c);
    }
}

/// The open form, if it's of the kind asked.
fn open_form(overlay: &mut Option<Overlay>, kind: fn(&FormKind) -> bool) -> Option<&mut Form> {
    match overlay {
        Some(Overlay::Form(form)) if kind(&form.kind) => Some(form.as_mut()),
        _ => None,
    }
}

/// Whether Delete account is with the server: then being signed out because
/// the account was deleted is what was asked for.
pub fn deleting(state: &State) -> bool {
    match &state.overlay {
        Some(Overlay::Form(form)) => form.waiting && form.kind == FormKind::DeleteAccount { step: DeleteStep::Confirm },
        _ => false,
    }
}

/// The devices changed: the open Devices window asks for them again.
pub fn devices_changed(state: &mut State, effects: &mut Vec<Effect>) {
    if open_form(&mut state.overlay, |k| *k == FormKind::Devices).is_some_and(|f| f.loaded && !f.waiting) {
        effects.push(Effect::Call(Command::Devices));
    }
}

/// The core's answer to a form's command. It goes to the window that asked;
/// if that was closed meanwhile, what came of it is said in the corvo tab.
pub fn reply(state: &mut State, asked: Command, reply: Result<Reply, String>, effects: &mut Vec<Effect>) {
    // The core's text for a wrong password is for signing in.
    let reply = reply.map_err(|why| if why == say(&Error::WrongCredentials) { WRONG_PASSWORD.into() } else { why });
    let online = state.conn == Some(ConnState::Online);
    match asked {
        Command::SetDisplayName { name } => match (open_form(&mut state.overlay, |k| *k == FormKind::Name), reply) {
            (Some(form), Err(why)) => {
                form.waiting = false;
                form.say(Tone::Bad, why);
            }
            (None, Err(why)) => state.say(Tone::Bad, why),
            (form, Ok(_)) => {
                if form.is_some() {
                    state.overlay = None;
                }
                state.say(Tone::Good, format!("Your display name is now {name}."));
                state.name = name;
            }
        },
        Command::ChangePassword { .. } => match open_form(&mut state.overlay, |k| *k == FormKind::Password) {
            Some(form) => {
                form.waiting = false;
                form.done = true;
                match reply {
                    Ok(_) => form.say(Tone::Good, PASSWORD_CHANGED),
                    Err(why) => form.say(Tone::Bad, why),
                }
            }
            None => match reply {
                Ok(_) => state.say(Tone::Good, PASSWORD_CHANGED),
                Err(why) => state.say(Tone::Bad, why),
            },
        },
        Command::NewRecoveryCode { .. } => {
            match (reply, open_form(&mut state.overlay, |k| *k == FormKind::RecoveryCode)) {
                (Ok(Reply::RecoveryCode(code)), form) => {
                    let code = Secret::new(code.display());
                    match form {
                        Some(form) => {
                            form.waiting = false;
                            form.fields.clear();
                            form.note = None;
                            form.code = Some(code);
                        }
                        // The old code works no more: this one mustn't get lost.
                        None => {
                            let mut form = Form::recovery_code();
                            form.fields.clear();
                            form.code = Some(code);
                            state.overlay = Some(Overlay::form(form));
                        }
                    }
                }
                (Ok(_), _) => {}
                (Err(why), Some(form)) => {
                    form.waiting = false;
                    if let Some(field) = form.fields.first_mut() {
                        field.take();
                    }
                    form.say(Tone::Bad, why);
                }
                (Err(why), None) => state.say(Tone::Bad, why),
            }
        }
        Command::Devices => {
            let Some(form) = open_form(&mut state.overlay, |k| *k == FormKind::Devices) else { return };
            if !form.loaded {
                form.waiting = false;
                form.note = None;
            }
            match reply {
                Ok(Reply::Devices(devices)) => {
                    form.set_devices(devices);
                    form.offline = !online;
                }
                Ok(_) => {}
                Err(why) => {
                    form.loaded = true;
                    form.say(Tone::Bad, why);
                }
            }
        }
        Command::SignOutDevice { .. } => {
            let Some(form) = open_form(&mut state.overlay, |k| *k == FormKind::Devices) else {
                if let Err(why) = reply {
                    state.say(Tone::Bad, why);
                }
                return;
            };
            form.waiting = false;
            let name = form.asking.take().map(|d| device_name(&d)).unwrap_or_default();
            match reply {
                Ok(Reply::Devices(devices)) => {
                    form.set_devices(devices);
                    form.offline = false;
                    form.say(Tone::Good, format!("Signed out {name}."));
                }
                Ok(_) => {}
                Err(why) => form.say(Tone::Bad, why),
            }
        }
        Command::RenameDevice { name } => {
            let Some(form) = open_form(&mut state.overlay, |k| *k == FormKind::Devices) else {
                if let Err(why) = reply {
                    state.say(Tone::Bad, why);
                }
                return;
            };
            form.waiting = false;
            match reply {
                Ok(_) => {
                    form.fields.clear();
                    form.say(Tone::Good, format!("This device is now called {}.", clean(&name)));
                    effects.push(Effect::Call(Command::Devices));
                }
                Err(why) => form.say(Tone::Bad, why),
            }
        }
        Command::DeleteAccount { .. } => match reply {
            Ok(_) => effects.push(Effect::Leave(vec![ACCOUNT_DELETED.into()])),
            Err(why) => match open_form(&mut state.overlay, |k| matches!(k, FormKind::DeleteAccount { .. })) {
                Some(form) => {
                    form.waiting = false;
                    form.go_to(DeleteStep::Password, Field::typed("Password", true));
                    form.say(Tone::Bad, why);
                }
                None => state.say(Tone::Bad, why),
            },
        },
        _ => {}
    }
}

/// A device's name as shown: cleaned, on one line.
fn device_name(device: &DeviceInfo) -> String {
    let name = clean(&device.name.replace('\n', " "));
    if name.trim().is_empty() { "(no name)".into() } else { name.trim().to_string() }
}

fn kind(kind: DeviceKind) -> &'static str {
    match kind {
        DeviceKind::Cli => "terminal",
        DeviceKind::Web => "web",
        DeviceKind::Phone => "phone",
    }
}

/// A time in Unix seconds, in local time.
fn when(secs: i64) -> String {
    Local.timestamp_opt(secs, 0).single().map_or_else(|| "?".into(), |t| t.format("%-d %b %H:%M").to_string())
}

/// The Devices table: a heading, then a row each, the chosen one marked `›`.
fn device_rows(form: &Form, inner: usize) -> Vec<(Zeroizing<String>, Tone)> {
    if form.devices.is_empty() {
        return if form.loaded { vec![(Zeroizing::new("No devices.".into()), Tone::Plain)] } else { Vec::new() };
    }
    let cells: Vec<Vec<String>> = form
        .devices
        .iter()
        .map(|d| {
            let this = if d.this_device { "this device" } else { "" };
            vec![cut(&device_name(d), DEVICE_NAME), kind(d.kind).into(), when(d.last_used), this.into()]
        })
        .collect();
    let mut lines = table(&["NAME", "TYPE", "LAST USED", ""], &cells).into_iter();
    let mut rows = Vec::new();
    if let Some(head) = lines.next() {
        rows.push((Zeroizing::new(cut(&format!("  {head}"), inner)), Tone::Dim));
    }
    for (i, line) in lines.enumerate() {
        let mark = if i == form.selected { '›' } else { ' ' };
        rows.push((Zeroizing::new(cut(&format!("{mark} {line}"), inner)), Tone::Plain));
    }
    rows
}

/// The form's rows in `inner` columns, at most `room` of them, each with
/// its tone. Wiped after use: they may hold the new recovery code.
///
/// What the window says and its fields come first. Below them, wrapped,
/// what happened and the keys; the blank row and then the keys go first
/// when there's no room for everything.
pub fn rows(form: &Form, inner: usize, room: usize) -> Vec<(Zeroizing<String>, Tone)> {
    let wrapped = |text: &str, tone: Tone| -> Vec<(Zeroizing<String>, Tone)> {
        wrap(text, inner).into_iter().map(|r| (Zeroizing::new(r), tone)).collect()
    };
    let mut rows = Vec::new();
    for (text, tone) in form.text() {
        rows.extend(wrapped(text, tone));
    }
    if let Some(code) = form.code.as_ref().filter(|_| form.shows_code()) {
        // Never wrapped: it fits the narrowest window, and a copy made to
        // wrap it wouldn't be wiped.
        let mut row = Zeroizing::new(String::with_capacity(code.expose().len() + 4));
        row.push_str("    ");
        row.push_str(code.expose());
        rows.push((Zeroizing::new(String::new()), Tone::Plain));
        rows.push((row, Tone::Plain));
        rows.push((Zeroizing::new(String::new()), Tone::Plain));
        rows.extend(wrapped("It's the only way back in if you forget your password.", Tone::Plain));
    }
    if form.kind == FormKind::Devices && form.fields.is_empty() {
        rows.extend(device_rows(form, inner));
    }
    let width = form.fields.iter().map(|f| f.label.width()).max().unwrap_or(0);
    for (i, field) in form.fields.iter().enumerate() {
        rows.push((field_row(field, i == form.focus, width, inner), Tone::Plain));
    }
    let blank = vec![(Zeroizing::new(String::new()), Tone::Plain)];
    let note = form.note.as_ref().map_or_else(Vec::new, |n| wrapped(&clean(&n.text), n.tone));
    let hint = wrapped(form.hint(), Tone::Dim);
    // In the order they're dropped when there's no room.
    let mut extras = [blank, hint, note];
    let mut drop = 0;
    while rows.len() + extras.iter().map(Vec::len).sum::<usize>() > room && drop < extras.len() {
        extras[drop].clear();
        drop += 1;
    }
    let [blank, hint, note] = extras;
    rows.extend(blank.into_iter().chain(note).chain(hint));
    rows.truncate(room);
    rows
}

/// `› New   ana maria`: the field being typed in is marked, a masked one
/// shows `•`s, and a long value shows its end. What can't be drawn safely
/// shows as �.
fn field_row(field: &Field, focused: bool, label_width: usize, inner: usize) -> Zeroizing<String> {
    let mut row = Zeroizing::new(String::with_capacity(inner * 4 + 8));
    row.push(if focused { '›' } else { ' ' });
    row.push(' ');
    row.push_str(field.label);
    for _ in field.label.width()..label_width + 2 {
        row.push(' ');
    }
    let room = inner.saturating_sub(row.width()).max(1);
    if field.masked {
        for _ in 0..field.value.chars().count().min(room) {
            row.push('•');
        }
    } else {
        let mut start = field.value.len();
        let mut used = 0;
        for (i, c) in field.value.char_indices().rev() {
            used += c.width().unwrap_or(0);
            if used > room {
                break;
            }
            start = i;
        }
        for c in field.value[start..].chars() {
            row.push(if clean(c.encode_utf8(&mut [0; 4])).is_empty() { '\u{FFFD}' } else { c });
        }
    }
    row
}
