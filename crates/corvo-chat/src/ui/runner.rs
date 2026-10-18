//! Carries out [`Effect`]s: the core, the files and the terminal. What came
//! of each goes back to `update` as [`Msg::Done`].

use std::io::Write;
use std::path::PathBuf;

use corvo_chat_core::native::Roots;
use corvo_chat_core::{Strength, Update};
use ratatui::crossterm::execute;
use ratatui::crossterm::terminal::SetTitle;

use super::update::{Effect, Msg};

/// The public server, where accounts are made and signed in to.
pub const DEFAULT_SERVER: &str = "wss://chat.corvo.live/ws";

/// A server to sign up and sign in at, and the certificates it may present.
/// Tests pass one running in-process.
#[derive(Debug, Clone)]
#[allow(dead_code, reason = "signing up and in connect with it")]
pub struct Server {
    pub url: String,
    pub roots: Roots,
}

impl Default for Server {
    /// [`DEFAULT_SERVER`], with the usual public authorities.
    fn default() -> Server {
        Server { url: DEFAULT_SERVER.into(), roots: Roots::Web }
    }
}

/// What the loop does after an effect.
#[derive(Debug)]
pub enum Flow {
    /// Carry on, feeding this to `update` if there is one.
    Next(Option<Msg>),
    Quit,
    /// Leave, printing this.
    Exit(String),
    /// The account is deleted: leave, printing the goodbye and these.
    #[allow(dead_code, reason = "deleting the account leaves this way")]
    Leave(Vec<String>),
}

pub struct Runner<W: Write> {
    /// The data folder.
    #[allow(dead_code, reason = "signing in keeps its files here")]
    dir: PathBuf,
    #[allow(dead_code, reason = "signing up and in connect with it")]
    server: Server,
    #[allow(dead_code, reason = "the password steps use it")]
    strength: Strength,
    /// The terminal: window titles are written here.
    out: W,
}

impl<W: Write> Runner<W> {
    /// `dir` is the data folder; `server` is where to sign up and sign in.
    pub fn new(dir: PathBuf, server: Server, strength: Strength, out: W) -> Runner<W> {
        Runner { dir, server, strength, out }
    }

    /// Does `effect`. An error here is the terminal failing; everything else
    /// comes back as an `Outcome`.
    pub async fn apply(&mut self, effect: Effect) -> anyhow::Result<Flow> {
        match effect {
            Effect::Quit => Ok(Flow::Quit),
            Effect::Exit(why) => Ok(Flow::Exit(why)),
            Effect::SetTitle(title) => {
                execute!(self.out, SetTitle(title))?;
                Ok(Flow::Next(None))
            }
        }
    }

    /// The core's next update; waits forever until it runs.
    pub async fn next_update(&mut self) -> Option<Update> {
        std::future::pending().await
    }

    /// Stops the core, once it runs.
    pub async fn shutdown(&mut self) {}
}

#[cfg(test)]
mod tests {
    use super::*;

    fn runner() -> Runner<Vec<u8>> {
        Runner::new(PathBuf::from("data"), Server::default(), Strength::Normal, Vec::new())
    }

    #[test]
    fn the_default_server_is_the_public_one() {
        let server = Server::default();
        assert_eq!(server.url, "wss://chat.corvo.live/ws");
        assert!(matches!(server.roots, Roots::Web));
    }

    #[tokio::test]
    async fn the_title_goes_to_the_terminal_and_quitting_ends() {
        let mut runner = runner();
        assert!(matches!(runner.apply(Effect::SetTitle("corvo-chat".into())).await.unwrap(), Flow::Next(None)));
        assert!(String::from_utf8_lossy(&runner.out).contains("corvo-chat"), "an escape code with the title");
        assert!(matches!(runner.apply(Effect::Quit).await.unwrap(), Flow::Quit));
        let exit = runner.apply(Effect::Exit("broken".into())).await.unwrap();
        assert!(matches!(exit, Flow::Exit(why) if why == "broken"));
    }

    #[tokio::test]
    async fn no_updates_come_before_the_core_runs() {
        let mut runner = runner();
        let waited = tokio::time::timeout(std::time::Duration::from_millis(50), runner.next_update()).await;
        assert!(waited.is_err(), "still waiting");
    }
}
