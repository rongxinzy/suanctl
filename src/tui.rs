use std::io;

use crossterm::{
    cursor::Show,
    event::{DisableMouseCapture, EnableMouseCapture},
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use ratatui::{backend::CrosstermBackend, Terminal};

use crate::domain::DashboardSnapshot;
use crate::{
    app::{self, AppState, UiReportFormat},
    collectors::runtime::RuntimeCollection,
};

pub fn run(state: &mut AppState) -> io::Result<()> {
    let backend = CrosstermBackend::new(io::stdout());
    let mut terminal = Terminal::new(backend)?;
    enable_raw_mode()?;
    let mut session = TerminalSession::active();
    if let Err(error) = execute!(
        terminal.backend_mut(),
        EnterAlternateScreen,
        EnableMouseCapture
    ) {
        let _ = session.restore(&mut terminal);
        return Err(error);
    }
    let result = app::run(&mut terminal, state);
    session.restore(&mut terminal)?;
    result
}

pub fn run_with_refresh<F>(state: &mut AppState, refresh: F) -> io::Result<()>
where
    F: FnMut() -> DashboardSnapshot,
{
    let backend = CrosstermBackend::new(io::stdout());
    let mut terminal = Terminal::new(backend)?;
    enable_raw_mode()?;
    let mut session = TerminalSession::active();
    if let Err(error) = execute!(
        terminal.backend_mut(),
        EnterAlternateScreen,
        EnableMouseCapture
    ) {
        let _ = session.restore(&mut terminal);
        return Err(error);
    }

    let result = app::run_with_refresh(&mut terminal, state, refresh);
    session.restore(&mut terminal)?;
    result
}

pub fn run_with_actions<R, P, E, S>(
    state: &mut AppState,
    refresh: R,
    benchmark: P,
    export: E,
    remote_scan: S,
) -> io::Result<()>
where
    R: Fn() -> RuntimeCollection + Send + Sync + 'static,
    P: Fn() -> (RuntimeCollection, Result<String, String>) + Send + Sync + 'static,
    E: Fn(RuntimeCollection, UiReportFormat) -> Result<String, String> + Send + Sync + 'static,
    S: Fn(&str) -> (crate::domain::RemoteScanSnapshot, Result<String, String>)
        + Send
        + Sync
        + 'static,
{
    let backend = CrosstermBackend::new(io::stdout());
    let mut terminal = Terminal::new(backend)?;
    enable_raw_mode()?;
    let mut session = TerminalSession::active();
    if let Err(error) = execute!(
        terminal.backend_mut(),
        EnterAlternateScreen,
        EnableMouseCapture
    ) {
        let _ = session.restore(&mut terminal);
        return Err(error);
    }

    let result = app::run_with_actions(
        &mut terminal,
        state,
        refresh,
        benchmark,
        export,
        remote_scan,
    );
    session.restore(&mut terminal)?;
    result
}

struct TerminalSession {
    active: bool,
}

impl TerminalSession {
    const fn active() -> Self {
        Self { active: true }
    }

    fn restore<B: ratatui::backend::Backend + io::Write>(
        &mut self,
        terminal: &mut Terminal<B>,
    ) -> io::Result<()> {
        if !self.active {
            return Ok(());
        }

        let result = restore_terminal(terminal);
        if result.is_ok() {
            self.active = false;
        }
        result
    }
}

impl Drop for TerminalSession {
    fn drop(&mut self) {
        if self.active {
            let _ = disable_raw_mode();
            let mut stdout = io::stdout();
            let _ = execute!(stdout, LeaveAlternateScreen, DisableMouseCapture, Show);
        }
    }
}

fn restore_terminal<B: ratatui::backend::Backend + io::Write>(
    terminal: &mut Terminal<B>,
) -> io::Result<()> {
    disable_raw_mode()?;
    execute!(
        terminal.backend_mut(),
        LeaveAlternateScreen,
        DisableMouseCapture,
        Show
    )?;
    terminal.show_cursor()
}
