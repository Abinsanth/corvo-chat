use std::io::stdout;

use anyhow::Context;
use clap::Parser;
use corvo_chat_core::Strength;

use self::files::{KEY_FILE, data_dir, home_dir, private_dir, username};
use self::ui::Ending;
use self::ui::runner::{Runner, Server};
use self::ui::start::Start;
use self::ui::state::State;

mod files;
mod logging;
mod selfupdate;
mod ui;

/// End-to-end encrypted chat in your terminal.
#[derive(Parser)]
#[command(version)]
struct Args {}

fn main() -> anyhow::Result<()> {
    Args::parse();
    let runtime = tokio::runtime::Runtime::new()?;
    ui::install_panic_hook();
    let dir = data_dir().context("couldn't tell where your home folder is; set HOME (APPDATA on Windows)")?;
    private_dir(&dir).with_context(|| format!("couldn't make the data folder {}", dir.display()))?;
    logging::start(&dir);
    let start = Start::new(dir.join(KEY_FILE).exists(), username(&dir), None);
    let mut state = State::start(start);
    state.data_dir = Some(ui::window::shown_path(&dir, home_dir().as_deref()));
    let runner = Runner::new(dir, Server::default(), Strength::Normal, stdout());
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
