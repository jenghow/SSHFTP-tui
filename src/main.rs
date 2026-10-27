//! sshftp-tui: a cross-platform TUI SSH + SFTP client.
//!
//! Task layout:
//! * the UI task renders with ratatui and reads crossterm events
//! * [`network`] owns the SSH session and all remote I/O
//! * the two talk over MPSC channels only

use std::io::{self, Stdout};
use std::time::Duration;

use anyhow::Result;
use clap::Parser;
use crossterm::cursor::SetCursorStyle;
use crossterm::event::{Event, EventStream};
use crossterm::execute;
use crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};
use futures::StreamExt;
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use sshftp_tui::app::App;
use sshftp_tui::event::{Cmd, Evt};
use sshftp_tui::{network, saved, sshconfig, ui};
use tokio::sync::mpsc;

#[derive(Parser, Debug)]
#[command(
    name = "sshftp-tui",
    version,
    about = "TUI SSH + SFTP client (russh + ratatui)"
)]
struct Cli {
    /// Prefill the connection form: [user@]host[:port]
    target: Option<String>,
}

fn main() -> Result<()> {
    install_panic_hook();
    let cli = Cli::parse();
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;
    runtime.block_on(run(cli.target))
}

async fn run(prefill: Option<String>) -> Result<()> {
    let guard = TerminalGuard::init()?;
    let backend = CrosstermBackend::new(io::stdout());
    let mut terminal = Terminal::new(backend)?;
    terminal.clear()?;

    // UI -> network and network -> UI.
    let (tx_cmd, rx_cmd) = mpsc::channel(512);
    let (tx_evt, rx_evt) = mpsc::channel(4096);

    let network_task = tokio::spawn(network::run(rx_cmd, tx_evt));

    // `~/.ssh/config` and the app-owned saved-connections file are parsed once,
    // before the event loop: the UI task must not read the disk, and $HOME can
    // be a network mount. An alias on the command line is resolved by
    // `set_configs` immediately; saved entries win over config defaults.
    let ssh_config = sshconfig::SshConfig::load();
    let (saved_config, saved_path) = saved::load();
    let mut app = App::new(tx_cmd.clone(), prefill);
    app.set_configs(ssh_config, saved_config, saved_path);
    if let Ok(size) = terminal.size() {
        app.on_resize(size.width, size.height);
    }
    // Populate the local pane straight away; the connection form is on screen.
    let path = app.local.cwd.clone();
    let _ = tx_cmd.send(Cmd::LocalList { path }).await;

    let mut events = EventStream::new();
    let result = ui_loop(&mut terminal, &mut app, &mut events, rx_evt).await;

    // Graceful shutdown: close the SSH session, drop the command channel so the
    // network task leaves its loop, and give it a moment to say goodbye.
    let _ = tx_cmd.send(Cmd::Disconnect).await;
    drop(app);
    drop(tx_cmd);
    let _ = tokio::time::timeout(Duration::from_secs(2), network_task).await;
    drop(guard);
    result
}

async fn ui_loop(
    terminal: &mut Terminal<CrosstermBackend<Stdout>>,
    app: &mut App,
    events: &mut EventStream,
    mut rx_evt: mpsc::Receiver<Evt>,
) -> Result<()> {
    // Drives the SSH caret blink: two redraws per second are enough, and
    // `tick` is cancel-safe so it can sit in the same `select!`. The first
    // phase starts at zero, so `App::blink_on`'s initial "solid" is really the
    // state the app opens with.
    let mut blink = tokio::time::interval_at(
        tokio::time::Instant::now() + Duration::from_millis(500),
        Duration::from_millis(500),
    );
    blink.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

    loop {
        terminal.draw(|frame| ui::draw(frame, app))?;

        tokio::select! {
            maybe_event = events.next() => match maybe_event {
                Some(Ok(Event::Key(key))) => app.on_key(key).await,
                Some(Ok(Event::Resize(width, height))) => app.on_resize(width, height),
                Some(Ok(_)) => {}
                Some(Err(_)) | None => break,
            },
            Some(event) = rx_evt.recv() => app.on_event(event).await,
            _ = blink.tick() => app.blink_tick(),
        }

        if app.should_quit {
            break;
        }
    }
    Ok(())
}

/// Puts the terminal into raw mode + alternate screen and restores it on drop,
/// which covers both the normal exit path and an early `?` return.
struct TerminalGuard;

impl TerminalGuard {
    fn init() -> Result<Self> {
        enable_raw_mode()?;
        // A block is what the form fields are designed around: it sits on the
        // character at the insertion point, or on the blank cell right after
        // the last one, so it always reads as "typing happens here". The SSH
        // tab draws its own blinking block (see `ui::draw_terminal`) because
        // not every terminal honours this request.
        execute!(
            io::stdout(),
            EnterAlternateScreen,
            SetCursorStyle::BlinkingBlock
        )?;
        Ok(Self)
    }
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        let _ = disable_raw_mode();
        let _ = execute!(
            io::stdout(),
            SetCursorStyle::DefaultUserShape,
            LeaveAlternateScreen
        );
    }
}

/// A panic would otherwise leave the user in raw mode with no echo.
fn install_panic_hook() {
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let _ = disable_raw_mode();
        let _ = execute!(
            io::stdout(),
            SetCursorStyle::DefaultUserShape,
            LeaveAlternateScreen
        );
        previous(info);
    }));
}
