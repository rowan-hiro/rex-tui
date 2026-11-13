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
use discovery::resolve_local_server;
use std::io;

#[derive(Parser, Debug)]
#[command(
    name = "rex-tui",
    version,
    about = "TUI client for managing and attaching to local Rex sessions"
)]
struct Cli {
    /// Directly attach to a session by label, full ID, or ID suffix
    #[arg(short = 's', long = "session")]
    session: Option<String>,

    /// Explicit Rex server Unix socket endpoint (e.g., unix:///path/to/server.sock)
    #[arg(short = 'S', long = "server")]
    server: Option<String>,

    /// Do not automatically start a local Rex server if none is running
    #[arg(long = "no-autostart")]
    no_autostart: bool,
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();

    let server = resolve_local_server(cli.server.as_deref(), !cli.no_autostart).await?;
    let (client, event_rx) = RexClient::connect(server.socket_path.clone()).await?;
    let mut app = App::new(server, client, event_rx, cli.session.as_deref()).await?;

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
