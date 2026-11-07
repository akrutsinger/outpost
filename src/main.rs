mod app;
mod net;
mod theme;
mod ui;

use std::io::{self, stdout};

use ratatui::DefaultTerminal;
use ratatui::crossterm::event::{self, DisableMouseCapture, EnableMouseCapture};
use ratatui::crossterm::execute;
use tokio::sync::mpsc::{self, UnboundedReceiver, UnboundedSender};

/// Everything the main loop reacts to: terminal input and network activity.
pub enum Event {
    Input(event::Event),
    Net(net::NetEvent),
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
        terminal.draw(|frame| ui::render(frame, &mut app))?;
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
