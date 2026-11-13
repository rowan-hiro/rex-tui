mod app;
mod client;
mod discovery;
mod input;
mod layout;
mod terminal_pane;
mod types;
mod ui;

use anyhow::Result;
use app::App;
use clap::Parser;
use client::RexClient;
use crossterm::event::{
    DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste, EnableMouseCapture,
};
use crossterm::execute;
use discovery::resolve_server;
use std::io;

#[derive(Parser, Debug)]
#[command(
    name = "rex-tui",
    version,
    about = "TUI client for managing and attaching to local and remote Rex sessions (Unix, HTTP, SSH)"
)]
struct Cli {
    /// Directly attach to a session by label, full ID, or ID suffix
    #[arg(short = 's', long = "session")]
    session: Option<String>,

    /// Target Rex server: host label, http://host:port, ssh://[user@]host[:port], or unix:///path
    #[arg(short = 'S', long = "server")]
    server: Option<String>,

    /// Do not automatically start a missing Rex server (local or remote over SSH)
    #[arg(long = "no-autostart")]
    no_autostart: bool,
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    let autostart = !cli.no_autostart;

    let server = resolve_server(cli.server.as_deref(), autostart, false).await?;
    let (client, event_rx) = RexClient::connect(server.target.clone()).await?;
    let mut app = App::new(
        server,
        client,
        event_rx,
        cli.session.as_deref(),
        autostart,
    )
    .await?;

    install_panic_hook();

    let mut terminal = ratatui::init();
    let _ = execute!(io::stdout(), EnableMouseCapture, EnableBracketedPaste);

    let run_res = app.run(&mut terminal).await;

    let _ = execute!(io::stdout(), DisableBracketedPaste, DisableMouseCapture);
    ratatui::restore();

    run_res
}

fn install_panic_hook() {
    let original_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |panic_info| {
        let _ = execute!(io::stdout(), DisableBracketedPaste, DisableMouseCapture);
        ratatui::restore();
        original_hook(panic_info);
    }));
}
