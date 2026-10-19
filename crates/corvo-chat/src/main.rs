use std::io::stdout;

use anyhow::Context;
use clap::Parser;
use corvo_chat_core::Strength;

use self::cli::Args;
use self::files::{KEY_FILE, data_dir, home_dir, private_dir, username};
use self::selfupdate::Urls;
use self::ui::Ending;
use self::ui::runner::{Runner, Server};
use self::ui::start::Start;
use self::ui::state::State;

mod cli;
mod files;
mod logging;
mod selfupdate;
mod ui;

fn main() -> anyhow::Result<()> {
    let args = Args::parse();
    let runtime = tokio::runtime::Runtime::new()?;
    let exe = selfupdate::running_exe();
    if args.update {
        let exe = exe.context("couldn't find the running app")?;
        let stdin = std::io::stdin();
        if let Err(why) = runtime.block_on(cli::update(&Urls::default(), &exe, stdin.lock(), stdout())) {
            eprintln!("{why}");
            std::process::exit(1);
        }
        return Ok(());
    }
    // A build from a checkout doesn't look for releases.
    let updates = exe.ok().filter(|exe| !selfupdate::is_built_from_source(exe)).map(|_| Urls::default());
    ui::install_panic_hook();
    let dir = data_dir().context("couldn't tell where your home folder is; set HOME (APPDATA on Windows)")?;
    private_dir(&dir).with_context(|| format!("couldn't make the data folder {}", dir.display()))?;
    logging::start(&dir);
    let start = Start::new(dir.join(KEY_FILE).exists(), username(&dir), None);
    let mut state = State::start(start);
    state.data_dir = Some(ui::window::shown_path(&dir, home_dir().as_deref()));
    let runner = Runner::new(dir, Server::default(), Strength::Normal, updates, stdout());
    match runtime.block_on(ui::run(state, runner))? {
        Ending::Quit => println!("{}", ui::window::goodbye().join("\n")),
        Ending::Leave(notes) => {
            println!("{}\n", ui::window::goodbye().join("\n"));
            for note in notes {
                println!("{note}");
            }
        }
        Ending::Exit(why) => {
            eprintln!("{why}");
            std::process::exit(1);
        }
    }
    Ok(())
}
