//! Parses a typed line into a message or a command. Pure: it knows nothing
//! about tabs or people, so name and number arguments come back as typed.

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Cmd {
    Code,
    /// A number, as typed.
    Add(String),
    /// The number to invite, or none to list your open invites.
    Invite(Option<String>),
    Chat(String),
    Contacts,
    Requests,
    /// The request named, or the one in the open tab.
    Accept(Option<String>),
    /// The request named, or the one in the open tab.
    Reject(Option<String>),
    /// The person named, or the one in the open tab.
    Block(Option<String>),
    Unblock(String),
    Blocked,
    Remove(String),
    Verify(String),
    Report(String),
    Info(String),
    Profile,
    Close,
    Clear,
    About,
    Help,
    Quit,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Input {
    /// Nothing but spaces.
    Empty,
    /// A message for the open chat, as typed.
    Text(String),
    Cmd(Cmd),
    /// What was wrong with the command, ready to show.
    Wrong(String),
    /// A known command typed without what it needs, or with the wrong words:
    /// its name, for its [`Usage`] card.
    Usage(&'static str),
}

/// Every command name `parse` accepts, aliases included, in the order "did you
/// mean" prefers them.
pub const COMMANDS: &[&str] = &[
    "code", "add", "invite", "chat", "contacts", "requests", "accept", "reject", "block", "unblock", "blocked",
    "remove", "verify", "report", "info", "profile", "close", "clear", "about", "help", "h", "quit", "q",
];

/// What `/help` lists, by group: each command and what it does. A name is a
/// display name or a number.
pub const HELP: &[(&str, &[(&str, &str)])] = &[
    ("You", &[("/profile", "name, password, devices")]),
    (
        "Friends",
        &[
            ("/code", "your number"),
            ("/add <number>", "add a friend"),
            ("/invite [number]", "invite someone new"),
            ("/contacts", "contacts and requests"),
            ("/requests", "people waiting for you"),
            ("/blocked", "blocked people"),
            ("/accept [name]", "accept a request"),
            ("/reject [name]", "drop a request"),
            ("/block [name]", "block someone"),
            ("/unblock <name>", "unblock someone"),
            ("/remove <name>", "delete a contact"),
            ("/verify <name>", "compare safety codes"),
            ("/report <name>", "report someone"),
            ("/info <name>", "about a contact"),
        ],
    ),
    (
        "Chatting",
        &[("/chat <name>", "open a chat"), ("/close", "close this tab"), ("/clear", "clear the screen (Ctrl-L)")],
    ),
    ("App", &[("/about", "version and info"), ("/help  /h", "this list"), ("/quit  /q", "quit (Ctrl-C)")]),
];

/// The keys, for `/help`.
pub const KEYS: &[(&str, &str)] = &[
    ("Tab", "complete"),
    ("Shift-Tab, Alt-1..9", "switch tabs"),
    ("Alt+Enter", "new line"),
    ("PgUp/PgDn", "scroll"),
    ("↑/↓", "what you typed before"),
];

/// How a command is typed, what it does in one sentence, and an example:
/// the card shown when it's typed wrong.
#[derive(Debug, PartialEq, Eq)]
pub struct Usage {
    pub usage: &'static str,
    pub what: &'static str,
    pub example: Option<&'static str>,
}

const fn card(usage: &'static str, what: &'static str, example: Option<&'static str>) -> Usage {
    Usage { usage, what, example }
}

/// Each command's card, by its name (not an alias).
const USAGES: &[(&str, Usage)] = &[
    ("code", card("/code", "Shows your number, for friends to add you by.", None)),
    (
        "add",
        card("/add <number>", "Adds a friend by their number and opens a chat with them.", Some("/add 482 910 5573")),
    ),
    (
        "invite",
        card(
            "/invite [number]",
            "Makes an invite code for someone signing up; alone, lists your open invites.",
            Some("/invite 482 910 5573"),
        ),
    ),
    ("chat", card("/chat <name|number>", "Opens a chat with a contact or a request.", Some("/chat rahul"))),
    ("contacts", card("/contacts", "Lists your contacts A to Z, then your requests.", None)),
    ("requests", card("/requests", "Lists the people who messaged you but aren't contacts yet.", None)),
    (
        "accept",
        card(
            "/accept [name|number]",
            "Accepts a request: the one named, or the one in this tab.",
            Some("/accept rahul"),
        ),
    ),
    (
        "reject",
        card(
            "/reject [name|number]",
            "Drops a request without telling them: the one named, or the one in this tab.",
            Some("/reject rahul"),
        ),
    ),
    (
        "block",
        card("/block [name|number]", "Blocks someone: the one named, or the one in this tab.", Some("/block rahul")),
    ),
    ("unblock", card("/unblock <name|number>", "Lets someone you blocked reach you again.", Some("/unblock rahul"))),
    ("blocked", card("/blocked", "Lists the people you've blocked.", None)),
    (
        "remove",
        card(
            "/remove <name|number>",
            "Deletes a contact, and asks whether to delete your history too.",
            Some("/remove rahul"),
        ),
    ),
    (
        "verify",
        card(
            "/verify <name|number>",
            "Shows the safety code to compare with them, so you know no one is in the middle.",
            Some("/verify rahul"),
        ),
    ),
    (
        "report",
        card(
            "/report <name|number>",
            "Reports someone to the server, then asks for a reason and whether to block them too.",
            Some("/report rahul"),
        ),
    ),
    (
        "info",
        card("/info <name|number>", "Shows a contact's name, number and whether they're online.", Some("/info rahul")),
    ),
    ("profile", card("/profile", "Opens your profile, to change your name or password or see your devices.", None)),
    ("close", card("/close", "Closes this chat tab.", None)),
    ("clear", card("/clear", "Clears the screen, like Ctrl-L.", None)),
    ("about", card("/about", "Shows the version, where your data lives and where the source is.", None)),
    ("help", card("/help  /h", "Lists the commands and keys.", None)),
    ("quit", card("/quit  /q", "Quits the app, like Ctrl-C.", None)),
];

/// The card for the command called `name`, alias or not.
pub fn usage(name: &str) -> Option<&'static Usage> {
    card_of(name).map(|(_, usage)| usage)
}

/// The card for `name`, with the name it's kept under.
fn card_of(name: &str) -> Option<&'static (&'static str, Usage)> {
    let name = match name {
        "h" => "help",
        "q" => "quit",
        other => other,
    };
    USAGES.iter().find(|(n, _)| *n == name)
}

/// Names that were never commands here but people type anyway, and what they meant.
const MEANT: &[(&str, &str)] = &[("exit", "quit")];

pub fn parse(line: &str) -> Input {
    if line.trim().is_empty() {
        return Input::Empty;
    }
    if let Some(escaped) = line.strip_prefix("//") {
        return Input::Text(format!("/{escaped}"));
    }
    let Some(rest) = line.trim().strip_prefix('/') else {
        return Input::Text(line.to_string());
    };
    let (name, args) = match rest.split_once(char::is_whitespace) {
        Some((name, args)) => (name, args.trim()),
        None => (rest, ""),
    };
    let name = name.to_lowercase();
    let cmd = match name.as_str() {
        "" => return Input::Wrong("type a command after the /, or /help".into()),
        "code" => no_args(args, Cmd::Code),
        "contacts" => no_args(args, Cmd::Contacts),
        "requests" => no_args(args, Cmd::Requests),
        "blocked" => no_args(args, Cmd::Blocked),
        "profile" => no_args(args, Cmd::Profile),
        "close" => no_args(args, Cmd::Close),
        "clear" => no_args(args, Cmd::Clear),
        "about" => no_args(args, Cmd::About),
        "help" | "h" => no_args(args, Cmd::Help),
        "quit" | "q" => no_args(args, Cmd::Quit),
        "add" => named(args).map(Cmd::Add),
        "chat" => named(args).map(Cmd::Chat),
        "unblock" => named(args).map(Cmd::Unblock),
        "remove" => named(args).map(Cmd::Remove),
        "verify" => named(args).map(Cmd::Verify),
        "report" => named(args).map(Cmd::Report),
        "info" => named(args).map(Cmd::Info),
        "invite" => Ok(Cmd::Invite(optional(args))),
        "accept" => Ok(Cmd::Accept(optional(args))),
        "reject" => Ok(Cmd::Reject(optional(args))),
        "block" => Ok(Cmd::Block(optional(args))),
        other => Err(Some(unknown(other))),
    };
    match cmd {
        Ok(cmd) => Input::Cmd(cmd),
        Err(Some(why)) => Input::Wrong(why),
        // Every name that gets here has a card.
        Err(None) => Input::Usage(card_of(&name).map_or("help", |(name, _)| name)),
    }
}

/// A command, or why not: `None` for its usage card, else what was wrong.
type Parsed<T> = Result<T, Option<String>>;

fn no_args(args: &str, cmd: Cmd) -> Parsed<Cmd> {
    if args.is_empty() { Ok(cmd) } else { Err(None) }
}

fn named(args: &str) -> Parsed<String> {
    if args.is_empty() { Err(None) } else { Ok(args.to_string()) }
}

fn optional(args: &str) -> Option<String> {
    (!args.is_empty()).then(|| args.to_string())
}

/// "unknown command", with a "did you mean" when a known name is close.
fn unknown(name: &str) -> String {
    match suggest(name) {
        Some(known) => format!("unknown command /{name}: did you mean /{known}?"),
        None => format!("unknown command /{name}, try /help"),
    }
}

/// What someone probably meant: the closest command within 2 edits, and closer
/// than the name is long so one letter doesn't guess. Ties go to the earlier
/// entry in `COMMANDS`.
fn suggest(name: &str) -> Option<&'static str> {
    if let Some((_, meant)) = MEANT.iter().find(|(typed, _)| *typed == name) {
        return Some(meant);
    }
    let len = name.chars().count();
    COMMANDS
        .iter()
        .map(|c| (distance(name, c), *c))
        .filter(|(d, _)| *d <= 2 && *d < len)
        .min_by_key(|(d, _)| *d)
        .map(|(_, c)| c)
}

/// Levenshtein distance in chars.
fn distance(a: &str, b: &str) -> usize {
    let b: Vec<char> = b.chars().collect();
    let mut row: Vec<usize> = (0..=b.len()).collect();
    for (i, ca) in a.chars().enumerate() {
        let mut diag = row[0];
        row[0] = i + 1;
        for (j, cb) in b.iter().enumerate() {
            let next = (diag + usize::from(ca != *cb)).min(row[j] + 1).min(row[j + 1] + 1);
            diag = row[j + 1];
            row[j + 1] = next;
        }
    }
    row[b.len()]
}

impl Cmd {
    /// How it's typed.
    pub fn typed(&self) -> &'static str {
        match self {
            Cmd::Code => "/code",
            Cmd::Add(_) => "/add",
            Cmd::Invite(_) => "/invite",
            Cmd::Chat(_) => "/chat",
            Cmd::Contacts => "/contacts",
            Cmd::Requests => "/requests",
            Cmd::Accept(_) => "/accept",
            Cmd::Reject(_) => "/reject",
            Cmd::Block(_) => "/block",
            Cmd::Unblock(_) => "/unblock",
            Cmd::Blocked => "/blocked",
            Cmd::Remove(_) => "/remove",
            Cmd::Verify(_) => "/verify",
            Cmd::Report(_) => "/report",
            Cmd::Info(_) => "/info",
            Cmd::Profile => "/profile",
            Cmd::Close => "/close",
            Cmd::Clear => "/clear",
            Cmd::About => "/about",
            Cmd::Help => "/help",
            Cmd::Quit => "/quit",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cmd(line: &str) -> Cmd {
        match parse(line) {
            Input::Cmd(cmd) => cmd,
            other => panic!("{line:?} parsed as {other:?}"),
        }
    }

    fn wrong(line: &str) -> String {
        match parse(line) {
            Input::Wrong(why) => why,
            other => panic!("{line:?} parsed as {other:?}"),
        }
    }

    #[test]
    fn every_command_is_in_the_help_once() {
        let usages: Vec<&str> =
            HELP.iter().flat_map(|(_, commands)| commands.iter().map(|(usage, _)| *usage)).collect();
        for name in COMMANDS {
            let slash = format!("/{name}");
            let found = usages.iter().filter(|u| u.split_whitespace().any(|word| word == slash)).count();
            assert_eq!(found, 1, "{slash} in the help: {usages:?}");
        }
        assert_eq!(HELP.iter().map(|(group, _)| *group).collect::<Vec<_>>(), ["You", "Friends", "Chatting", "App"]);
    }

    #[test]
    fn every_command_has_a_usage() {
        for name in COMMANDS {
            let card = usage(name).unwrap_or_else(|| panic!("/{name} has no usage"));
            let slash = format!("/{name}");
            assert!(card.usage.split_whitespace().any(|w| w == slash), "/{name}: {card:?}");
            let one = card.what.ends_with('.') && !card.what.contains(". ");
            assert!(one, "/{name}: one sentence: {:?}", card.what);
            assert!(card.example.is_none_or(|e| e.starts_with(&slash)), "/{name}: {card:?}");
        }
        assert_eq!(usage("h"), usage("help"), "aliases share it");
        assert_eq!(usage("q"), usage("quit"));
        assert_eq!(usage("nope"), None);
    }

    #[test]
    fn a_command_missing_its_name_gives_its_usage() {
        assert_eq!(parse("/chat"), Input::Usage("chat"));
        assert_eq!(parse("/add"), Input::Usage("add"));
        assert_eq!(parse("/q now"), Input::Usage("quit"), "under its name, not the alias");
    }

    #[test]
    fn plain_text_empty_lines_and_the_slash_escape() {
        assert_eq!(parse("hello there "), Input::Text("hello there ".into()));
        assert_eq!(parse("  \n "), Input::Empty);
        assert_eq!(parse("//shrug"), Input::Text("/shrug".into()));
        assert_eq!(parse("a /b"), Input::Text("a /b".into()));
    }

    #[test]
    fn commands_without_arguments() {
        for (line, want) in [
            ("/code", Cmd::Code),
            ("/contacts", Cmd::Contacts),
            ("/requests", Cmd::Requests),
            ("/blocked", Cmd::Blocked),
            ("/profile", Cmd::Profile),
            ("/close", Cmd::Close),
            ("/clear", Cmd::Clear),
            ("/about", Cmd::About),
            ("/help", Cmd::Help),
            ("/h", Cmd::Help),
            ("/quit", Cmd::Quit),
            ("/q", Cmd::Quit),
            ("  /quit  ", Cmd::Quit),
            ("/QUIT", Cmd::Quit),
        ] {
            assert_eq!(cmd(line), want, "{line}");
        }
    }

    #[test]
    fn commands_without_arguments_refuse_extra_ones() {
        for (line, name) in [
            ("/code now", "code"),
            ("/contacts all", "contacts"),
            ("/requests x", "requests"),
            ("/blocked x", "blocked"),
            ("/profile name", "profile"),
            ("/close bob", "close"),
            ("/clear all", "clear"),
            ("/help me", "help"),
            ("/q now", "quit"),
        ] {
            assert_eq!(parse(line), Input::Usage(name), "{line}");
        }
    }

    #[test]
    fn commands_that_need_a_name_or_number() {
        assert_eq!(cmd("/add 482 910 5573"), Cmd::Add("482 910 5573".into()), "spaces and all");
        assert_eq!(cmd("/chat rahul"), Cmd::Chat("rahul".into()));
        assert_eq!(cmd("/chat  ana maria "), Cmd::Chat("ana maria".into()));
        assert_eq!(cmd("/unblock 4829105573"), Cmd::Unblock("4829105573".into()));
        assert_eq!(cmd("/remove rahul"), Cmd::Remove("rahul".into()));
        assert_eq!(cmd("/verify rahul"), Cmd::Verify("rahul".into()));
        assert_eq!(cmd("/report rahul"), Cmd::Report("rahul".into()));
        assert_eq!(cmd("/info rahul"), Cmd::Info("rahul".into()));
        for name in ["add", "chat", "unblock", "remove", "verify", "report", "info"] {
            assert_eq!(parse(&format!("/{name} ")), Input::Usage(name), "/{name}");
        }
    }

    #[test]
    fn some_take_a_name_or_mean_this_tab() {
        assert_eq!(cmd("/accept"), Cmd::Accept(None));
        assert_eq!(cmd("/accept rahul"), Cmd::Accept(Some("rahul".into())));
        assert_eq!(cmd("/reject"), Cmd::Reject(None));
        assert_eq!(cmd("/reject 482 910 5573"), Cmd::Reject(Some("482 910 5573".into())));
        assert_eq!(cmd("/block"), Cmd::Block(None));
        assert_eq!(cmd("/block ana maria"), Cmd::Block(Some("ana maria".into())));
        assert_eq!(cmd("/invite"), Cmd::Invite(None), "lists the open invites");
        assert_eq!(cmd("/invite 482 910 5573"), Cmd::Invite(Some("482 910 5573".into())));
    }

    #[test]
    fn the_twelve_words_and_update_are_unknown() {
        for line in ["/update", "/update yes", "/phrase", "/passphrase", "/name Ana", "/recover"] {
            assert!(wrong(line).starts_with("unknown command"), "{line}: {}", wrong(line));
        }
    }

    #[test]
    fn typos_get_a_did_you_mean() {
        assert_eq!(wrong("/acept"), "unknown command /acept: did you mean /accept?");
        assert_eq!(wrong("/acept rahul"), "unknown command /acept: did you mean /accept?");
        assert_eq!(wrong("/hepl"), "unknown command /hepl: did you mean /help?");
        assert_eq!(wrong("/contact"), "unknown command /contact: did you mean /contacts?");
        assert_eq!(wrong("/verfy"), "unknown command /verfy: did you mean /verify?");
        assert_eq!(wrong("/exit"), "unknown command /exit: did you mean /quit?");
        assert_eq!(wrong("/x"), "unknown command /x, try /help");
        assert_eq!(wrong("/frobnicate"), "unknown command /frobnicate, try /help");
        assert_eq!(wrong("/"), "type a command after the /, or /help");
    }

    #[test]
    fn every_listed_command_parses_and_says_how_it_is_typed() {
        for name in COMMANDS {
            let line = match *name {
                "add" => "/add 482 910 5573".to_string(),
                "chat" | "unblock" | "remove" | "verify" | "report" | "info" => format!("/{name} rahul"),
                _ => format!("/{name}"),
            };
            let parsed = cmd(&line);
            let typed = parsed.typed();
            assert!(line.starts_with(typed) || ["/h", "/q"].contains(&line.as_str()), "{line} says {typed}");
        }
    }
}
