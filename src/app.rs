use std::time::Duration;
use std::{
    io,
    sync::{mpsc, Arc},
    thread,
};

use crossterm::event::{KeyCode, KeyEvent};
use ratatui::{backend::Backend, Terminal};

use crate::{
    collectors::runtime::RuntimeCollection,
    domain::{CollectionIssue, DashboardSnapshot, HealthStatus, UiPage},
    event::{Event, EventHandler},
    ui,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UiOperationKind {
    P2pBenchmark,
    ReportExport,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum UiReportFormat {
    Json,
    Jsonl,
    #[default]
    Markdown,
}

impl UiReportFormat {
    pub const fn label(self) -> &'static str {
        match self {
            Self::Json => "JSON",
            Self::Jsonl => "JSONL",
            Self::Markdown => "Markdown",
        }
    }

    pub const fn extension(self) -> &'static str {
        match self {
            Self::Json => "json",
            Self::Jsonl => "jsonl",
            Self::Markdown => "md",
        }
    }

    pub const fn next(self) -> Self {
        match self {
            Self::Json => Self::Jsonl,
            Self::Jsonl => Self::Markdown,
            Self::Markdown => Self::Json,
        }
    }
}

impl UiOperationKind {
    pub const fn label(self) -> &'static str {
        match self {
            Self::P2pBenchmark => "P2P 实测速率",
            Self::ReportExport => "报告导出",
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub enum UiOperationState {
    #[default]
    Idle,
    Confirm(UiOperationKind),
    Running(UiOperationKind),
    Succeeded {
        kind: UiOperationKind,
        message: String,
    },
    Failed {
        kind: UiOperationKind,
        message: String,
    },
}

#[derive(Debug)]
pub struct AppState {
    pub page: UiPage,
    pub show_help: bool,
    /// Number of explicit refresh requests made with `r`.
    pub refresh_count: u64,
    /// True after `r` until a future collector acknowledges the request.
    pub refresh_requested: bool,
    /// UI redraw ticks. This is deliberately separate from refresh requests.
    pub ui_tick_count: u64,
    /// Last terminal size observed by the event loop.
    pub terminal_size: Option<(u16, u16)>,
    pub operation: UiOperationState,
    pub report_format: UiReportFormat,
    pub collection_status: HealthStatus,
    pub collection_issues: Vec<CollectionIssue>,
    pub snapshot: DashboardSnapshot,
    should_quit: bool,
}

impl AppState {
    pub fn new(snapshot: DashboardSnapshot) -> Self {
        Self {
            page: UiPage::Overview,
            show_help: false,
            refresh_count: 0,
            refresh_requested: false,
            ui_tick_count: 0,
            terminal_size: None,
            operation: UiOperationState::Idle,
            report_format: UiReportFormat::default(),
            collection_status: HealthStatus::Unknown,
            collection_issues: Vec::new(),
            snapshot,
            should_quit: false,
        }
    }

    pub fn from_collection(collection: RuntimeCollection) -> Self {
        let mut state = Self::new(collection.snapshot);
        state.collection_status = collection.status;
        state.collection_issues = collection.issues;
        state
    }

    pub fn collection(&self) -> RuntimeCollection {
        RuntimeCollection {
            snapshot: self.snapshot.clone(),
            issues: self.collection_issues.clone(),
            status: self.collection_status,
        }
    }

    pub fn replace_collection(&mut self, collection: RuntimeCollection) {
        self.snapshot = collection.snapshot;
        self.collection_status = collection.status;
        self.collection_issues = collection.issues;
    }

    pub const fn should_quit(&self) -> bool {
        self.should_quit
    }

    pub fn handle_key(&mut self, key: KeyEvent) {
        if self.handle_operation_key(key.code) {
            return;
        }
        match key.code {
            KeyCode::Char('q') | KeyCode::Esc => self.should_quit = true,
            KeyCode::Char('?') => self.show_help = !self.show_help,
            KeyCode::Char('r') => self.request_refresh(),
            KeyCode::Char('b') => self.request_operation(UiOperationKind::P2pBenchmark),
            KeyCode::Char('e') => self.request_operation(UiOperationKind::ReportExport),
            KeyCode::Char('m') => self.report_format = self.report_format.next(),
            KeyCode::Char(digit) => {
                if let Some(page) = UiPage::from_digit(digit) {
                    self.page = page;
                }
            }
            KeyCode::Left => self.page = self.page.previous(),
            KeyCode::Right => self.page = self.page.next(),
            _ => {}
        }
    }

    fn handle_operation_key(&mut self, key: KeyCode) -> bool {
        match self.operation {
            UiOperationState::Confirm(kind) => match key {
                KeyCode::Enter | KeyCode::Char('y') | KeyCode::Char('Y') => {
                    self.operation = UiOperationState::Running(kind);
                    true
                }
                KeyCode::Esc | KeyCode::Char('n') | KeyCode::Char('N') => {
                    self.operation = UiOperationState::Idle;
                    true
                }
                _ => true,
            },
            UiOperationState::Running(_) => true,
            UiOperationState::Succeeded { .. } | UiOperationState::Failed { .. } => match key {
                KeyCode::Enter | KeyCode::Esc => {
                    self.operation = UiOperationState::Idle;
                    true
                }
                _ => false,
            },
            UiOperationState::Idle => false,
        }
    }

    fn request_operation(&mut self, kind: UiOperationKind) {
        if !matches!(self.operation, UiOperationState::Running(_)) {
            self.operation = UiOperationState::Confirm(kind);
        }
    }

    fn take_started_operation(&self) -> Option<UiOperationKind> {
        match self.operation {
            UiOperationState::Running(kind) => Some(kind),
            _ => None,
        }
    }

    fn complete_operation(&mut self, kind: UiOperationKind, result: Result<String, String>) {
        self.operation = match result {
            Ok(message) => UiOperationState::Succeeded { kind, message },
            Err(message) => UiOperationState::Failed { kind, message },
        };
    }

    pub fn refresh(&mut self) {
        self.refresh_count = self.refresh_count.saturating_add(1);
        self.refresh_requested = true;
    }

    pub fn tick(&mut self) {
        self.ui_tick_count = self.ui_tick_count.saturating_add(1);
    }

    pub fn request_refresh(&mut self) {
        self.refresh_count = self.refresh_count.saturating_add(1);
        self.refresh_requested = true;
    }

    pub fn refresh_if_requested<F>(&mut self, mut refresh: F)
    where
        F: FnMut() -> DashboardSnapshot,
    {
        if self.refresh_requested {
            self.snapshot = refresh();
            self.refresh_requested = false;
        }
    }

    pub fn refresh_collection_if_requested<F>(&mut self, mut refresh: F)
    where
        F: FnMut() -> RuntimeCollection,
    {
        if self.refresh_requested {
            self.replace_collection(refresh());
            self.refresh_requested = false;
        }
    }

    pub fn handle_event(&mut self, event: Event) {
        match event {
            Event::Key(key) => self.handle_key(key),
            Event::Tick => self.tick(),
            Event::Resize(width, height) => self.terminal_size = Some((width, height)),
        }
    }
}

pub fn run<B: Backend>(terminal: &mut Terminal<B>, state: &mut AppState) -> io::Result<()> {
    let mut events = EventHandler::new(Duration::from_millis(250));
    while !state.should_quit() {
        terminal.draw(|frame| ui::draw(frame, state))?;
        state.handle_event(events.next_event()?);
    }
    Ok(())
}

/// Run the event loop with an injected, synchronous snapshot refresh.
///
/// The UI only invokes this callback after an explicit `r` key event. Ticks
/// remain redraw signals and never masquerade as collection refreshes.
pub fn run_with_refresh<B, F>(
    terminal: &mut Terminal<B>,
    state: &mut AppState,
    mut refresh: F,
) -> io::Result<()>
where
    B: Backend,
    F: FnMut() -> DashboardSnapshot,
{
    let mut events = EventHandler::new(Duration::from_millis(250));

    while !state.should_quit() {
        terminal.draw(|frame| ui::draw(frame, state))?;
        state.handle_event(events.next_event()?);
        state.refresh_if_requested(&mut refresh);
    }

    Ok(())
}

/// Full interactive entrypoint. Refresh remains a synchronous explicit read;
/// benchmark and export execute in background threads after a visible
/// confirmation, keeping the Ratatui event loop responsive.
pub fn run_with_actions<B, R, P, E>(
    terminal: &mut Terminal<B>,
    state: &mut AppState,
    mut refresh: R,
    benchmark: P,
    export: E,
) -> io::Result<()>
where
    B: Backend,
    R: FnMut() -> RuntimeCollection,
    P: Fn() -> (RuntimeCollection, Result<String, String>) + Send + Sync + 'static,
    E: Fn(RuntimeCollection, UiReportFormat) -> Result<String, String> + Send + Sync + 'static,
{
    let benchmark = Arc::new(benchmark);
    let export = Arc::new(export);
    let (sender, receiver) = mpsc::channel::<(
        UiOperationKind,
        Result<String, String>,
        Option<RuntimeCollection>,
    )>();
    let mut running_operation = None;
    let mut events = EventHandler::new(Duration::from_millis(250));

    while !state.should_quit() {
        while let Ok((kind, result, collection)) = receiver.try_recv() {
            if let Some(collection) = collection {
                state.replace_collection(collection);
            }
            state.complete_operation(kind, result);
            running_operation = None;
        }

        terminal.draw(|frame| ui::draw(frame, state))?;
        state.handle_event(events.next_event()?);
        state.refresh_collection_if_requested(&mut refresh);

        let started = state.take_started_operation();
        if let Some(kind) = started.filter(|kind| running_operation != Some(*kind)) {
            running_operation = Some(kind);
            let sender = sender.clone();
            match kind {
                UiOperationKind::P2pBenchmark => {
                    let benchmark = Arc::clone(&benchmark);
                    thread::spawn(move || {
                        let (collection, result) = benchmark();
                        let _ = sender.send((kind, result, Some(collection)));
                    });
                }
                UiOperationKind::ReportExport => {
                    let export = Arc::clone(&export);
                    let collection = state.collection();
                    let format = state.report_format;
                    thread::spawn(move || match export(collection, format) {
                        Ok(path) => {
                            let _ = sender.send((kind, Ok(format!("报告已写入：{path}")), None));
                        }
                        Err(message) => {
                            let _ = sender.send((kind, Err(message), None));
                        }
                    });
                }
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

    use super::{AppState, UiOperationKind, UiOperationState, UiReportFormat};
    use crate::{
        collectors::{demo, runtime::RuntimeCollection},
        domain::{CollectionIssue, HealthStatus, UiPage},
    };

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    #[test]
    fn numeric_and_arrow_keys_change_pages() {
        let mut state = AppState::new(demo::snapshot());
        state.handle_key(key(KeyCode::Char('3')));
        assert_eq!(state.page, UiPage::Services);
        state.handle_key(key(KeyCode::Left));
        assert_eq!(state.page, UiPage::Gpu);
        state.handle_key(key(KeyCode::Right));
        assert_eq!(state.page, UiPage::Services);
    }

    #[test]
    fn refresh_help_and_quit_shortcuts_change_only_app_state() {
        let mut state = AppState::new(demo::snapshot());
        state.handle_key(key(KeyCode::Char('?')));
        assert!(state.show_help);
        state.handle_key(key(KeyCode::Char('r')));
        assert_eq!(state.refresh_count, 1);
        assert!(state.refresh_requested);
        assert_eq!(state.ui_tick_count, 0);
        state.handle_key(key(KeyCode::Char('q')));
        assert!(state.should_quit());
    }

    #[test]
    fn automatic_ticks_do_not_claim_that_a_collection_refresh_happened() {
        let mut state = AppState::new(demo::snapshot());
        state.handle_event(crate::event::Event::Tick);
        state.handle_event(crate::event::Event::Tick);

        assert_eq!(state.ui_tick_count, 2);
        assert_eq!(state.refresh_count, 0);
        assert!(!state.refresh_requested);
    }

    #[test]
    fn resize_is_recorded_without_touching_snapshot_or_panicking() {
        let snapshot = demo::snapshot();
        let mut state = AppState::new(snapshot.clone());
        state.handle_event(crate::event::Event::Resize(80, 24));

        assert_eq!(state.terminal_size, Some((80, 24)));
        assert_eq!(state.snapshot, snapshot);
    }

    #[test]
    fn injected_refresh_runs_only_for_an_explicit_request() {
        let initial = demo::snapshot();
        let replacement = crate::collectors::demo::snapshot();
        let mut state = AppState::new(initial);
        let mut calls = 0;
        state.refresh_if_requested(|| {
            calls += 1;
            replacement.clone()
        });
        assert_eq!(calls, 0);
        state.request_refresh();
        state.refresh_if_requested(|| {
            calls += 1;
            replacement.clone()
        });
        assert_eq!(calls, 1);
        assert!(!state.refresh_requested);
    }

    #[test]
    fn collection_refresh_retains_status_and_collection_issues() {
        let initial = RuntimeCollection {
            snapshot: demo::snapshot(),
            issues: vec![CollectionIssue {
                collector: "initial".to_owned(),
                code: "initial_issue".to_owned(),
                status: HealthStatus::Warning,
                message: "初始采集问题".to_owned(),
            }],
            status: HealthStatus::Warning,
        };
        let replacement = RuntimeCollection {
            snapshot: demo::snapshot(),
            issues: vec![CollectionIssue {
                collector: "refresh".to_owned(),
                code: "refresh_issue".to_owned(),
                status: HealthStatus::Unavailable,
                message: "刷新采集问题".to_owned(),
            }],
            status: HealthStatus::Unavailable,
        };
        let mut state = AppState::from_collection(initial);
        state.request_refresh();
        state.refresh_collection_if_requested(|| replacement.clone());

        assert_eq!(state.collection_status, HealthStatus::Unavailable);
        assert_eq!(state.collection_issues, replacement.issues);
    }

    #[test]
    fn report_format_cycles_without_starting_an_operation() {
        let mut state = AppState::new(demo::snapshot());
        assert_eq!(state.report_format, UiReportFormat::Markdown);
        state.handle_key(key(KeyCode::Char('m')));
        assert_eq!(state.report_format, UiReportFormat::Json);
        state.handle_key(key(KeyCode::Char('m')));
        assert_eq!(state.report_format, UiReportFormat::Jsonl);
        assert_eq!(state.operation, UiOperationState::Idle);
    }

    #[test]
    fn benchmark_and_export_require_confirmation_and_block_competing_actions() {
        let mut state = AppState::new(demo::snapshot());
        state.handle_key(key(KeyCode::Char('b')));
        assert_eq!(
            state.operation,
            UiOperationState::Confirm(UiOperationKind::P2pBenchmark)
        );
        state.handle_key(key(KeyCode::Char('n')));
        assert_eq!(state.operation, UiOperationState::Idle);

        state.handle_key(key(KeyCode::Char('e')));
        state.handle_key(key(KeyCode::Enter));
        assert_eq!(
            state.operation,
            UiOperationState::Running(UiOperationKind::ReportExport)
        );
        state.handle_key(key(KeyCode::Char('r')));
        assert!(!state.refresh_requested);
        state.complete_operation(
            UiOperationKind::ReportExport,
            Ok("reports/test.md".to_owned()),
        );
        assert!(matches!(
            state.operation,
            UiOperationState::Succeeded {
                kind: UiOperationKind::ReportExport,
                ..
            }
        ));
    }
}
