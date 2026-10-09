//! The Tiphys terminal app.
//!
//! The app is a client of the agent's host. This thread owns the terminal
//! and a [`view::View`]; a second thread runs the host. Keys become requests
//! to the host, the host's events change the view, and the view is drawn.
//! Nothing here reads a key file, calls a model or writes a session: that is
//! all the host's.

#![forbid(unsafe_code)]

use std::io::Stdout;
use std::path::Path;
use std::sync::Arc;
use std::sync::mpsc::{Receiver, channel};
use std::time::{Duration, Instant};

use crossterm::event::{
    DisableBracketedPaste, EnableBracketedPaste, Event as TermEvent, KeyEventKind,
};
use crossterm::execute;
use crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use tiphys_core::host::{Host, Sink};
use tiphys_core::llm::ChatConnect;
use tiphys_core::proto::{Event, Request};
use tiphys_core::{Error, Result};
use tokio::sync::mpsc::{UnboundedSender, unbounded_channel};

pub mod draw;
pub mod input;
pub mod view;
pub mod wrap;

use view::View;

/// How long to wait for a key before looking for events from the host.
const POLL: Duration = Duration::from_millis(40);
/// How often the spinner moves.
const TICK: Duration = Duration::from_millis(90);

/// Runs the app until the owner leaves it.
pub fn run(home: &Path, user_home: &Path) -> Result<()> {
    let (requests, inbox) = unbounded_channel();
    let (outbox, events) = channel();
    let sink: Sink = Arc::new(move |event: &Event| {
        let _ = outbox.send(event.clone());
    });
    let host = Host::new(home, user_home, "terminal", Arc::new(ChatConnect), sink);
    let runtime = tokio::runtime::Runtime::new()
        .map_err(|e| Error::Io(format!("could not start the runtime: {e}")))?;
    let serving = std::thread::spawn(move || runtime.block_on(host.run(inbox)));

    let outcome = Screen::open().and_then(|mut screen| screen.run(&requests, &events));

    // With the requests closed the host stops any running turn and ends. The
    // terminal is already back to normal by now, so waiting costs nothing.
    drop(requests);
    let _ = serving.join();
    outcome
}

/// The terminal while the app has it. Dropping this gives it back, on every
/// way out, a panic included.
struct Screen {
    terminal: Terminal<CrosstermBackend<Stdout>>,
}

impl Screen {
    fn open() -> Result<Self> {
        let io = |e: std::io::Error| Error::Io(format!("the terminal: {e}"));
        enable_raw_mode().map_err(io)?;
        let mut out = std::io::stdout();
        if let Err(e) = execute!(out, EnterAlternateScreen, EnableBracketedPaste) {
            let _ = disable_raw_mode();
            return Err(io(e));
        }
        let terminal = Terminal::new(CrosstermBackend::new(out)).map_err(io)?;
        Ok(Self { terminal })
    }

    fn run(&mut self, requests: &UnboundedSender<Request>, events: &Receiver<Event>) -> Result<()> {
        let io = |e: std::io::Error| Error::Io(format!("the terminal: {e}"));
        let send = |sent: Vec<Request>| {
            for request in sent {
                let _ = requests.send(request);
            }
        };
        let mut view = View::default();
        send(vec![Request::Hello]);
        let mut dirty = true;
        let mut ticked = Instant::now();
        while !view.quit {
            while let Ok(event) = events.try_recv() {
                send(view::apply(&mut view, &event));
                dirty = true;
            }
            if ticked.elapsed() >= TICK {
                view.tick += 1;
                ticked = Instant::now();
                dirty = true;
            }
            if dirty {
                self.terminal
                    .draw(|frame| draw::draw(&view, frame))
                    .map_err(io)?;
                dirty = false;
            }
            if !crossterm::event::poll(POLL).map_err(io)? {
                continue;
            }
            match crossterm::event::read().map_err(io)? {
                TermEvent::Key(key) if key.kind != KeyEventKind::Release => {
                    send(view::key(&mut view, key));
                }
                TermEvent::Paste(text) => view::paste(&mut view, &text),
                _ => {}
            }
            dirty = true;
        }
        Ok(())
    }
}

impl Drop for Screen {
    fn drop(&mut self) {
        let _ = disable_raw_mode();
        let _ = execute!(
            self.terminal.backend_mut(),
            DisableBracketedPaste,
            LeaveAlternateScreen
        );
        let _ = self.terminal.show_cursor();
    }
}
