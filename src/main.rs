mod app;
mod detect;
mod net;
mod pty;
mod text_input;
mod theme;
mod ui;

use std::io::{self, stdout};
use std::time::Duration;

use ratatui::DefaultTerminal;
use ratatui::crossterm::event::{self, DisableMouseCapture, EnableMouseCapture};
use ratatui::crossterm::execute;
use tokio::sync::mpsc::{self, UnboundedReceiver, UnboundedSender};

/// How often terminal tabs are checked for what they're connected to.
const DETECT_EVERY: Duration = Duration::from_millis(500);

/// Everything the main loop reacts to: terminal input, network activity,
/// the programs in terminal tabs, and detecting what those connect to.
pub enum Event {
    Input(event::Event),
    Net(net::NetEvent),
    Pty(pty::PtyEvent),
    /// Time to check terminal tabs' foreground commands.
    Tick,
    Resolved(detect::Resolved),
}

#[tokio::main]
async fn main() -> io::Result<()> {
    let (tx, mut rx) = mpsc::unbounded_channel();

    // crossterm's reader is blocking, so it lives on its own thread.
    let input_tx = tx.clone();
    std::thread::spawn(move || {
        while let Ok(ev) = event::read() {
            if input_tx.send(Event::Input(ev)).is_err() {
                break;
            }
        }
    });

    let tick_tx = tx.clone();
    tokio::spawn(async move {
        let mut ticks = tokio::time::interval(DETECT_EVERY);
        loop {
            ticks.tick().await;
            if tick_tx.send(Event::Tick).is_err() {
                break;
            }
        }
    });

    let mut terminal = ratatui::init();
    execute!(stdout(), EnableMouseCapture)?;
    let result = run(&mut terminal, &mut rx, tx).await;
    let _ = execute!(stdout(), DisableMouseCapture);
    ratatui::restore();
    result
}

async fn run(
    terminal: &mut DefaultTerminal,
    rx: &mut UnboundedReceiver<Event>,
    tx: UnboundedSender<Event>,
) -> io::Result<()> {
    let mut app = app::App::new(tx);
    loop {
        terminal.draw(|frame| {
            app.fit_terminals(ui::terminal_area(frame.area(), &app));
            ui::render(frame, &mut app);
        })?;
        let Some(ev) = rx.recv().await else { break };
        app.handle(ev);
        // Coalesce bursts (e.g. a flood of incoming data) into a single redraw.
        while let Ok(ev) = rx.try_recv() {
            app.handle(ev);
        }
        if app.quit {
            break;
        }
    }
    Ok(())
}
