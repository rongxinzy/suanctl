use std::time::Duration;
use std::{
    io,
    sync::{
        atomic::{AtomicBool, Ordering},
        mpsc, Arc,
    },
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
    RemoteScan,
    Refresh,
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
            Self::RemoteScan => "远程设备扫描",
            Self::Refresh => "采集刷新",
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
    /// Per-page vertical scroll offset（↑/↓/PgUp/PgDn）。
    scroll: std::collections::BTreeMap<UiPage, u16>,
    /// 是否处于 `/` 过滤输入模式。
    filter_mode: bool,
    /// 过滤关键字（实时生效，作用于日志/诊断页）。
    filter_buffer: String,
    /// 是否在远程扫描目标选择模式（s 键进入）。
    remote_target_mode: bool,
    /// 候选列表中的当前光标索引。
    remote_selection: usize,
    /// 远程扫描目标别名输入缓冲。
    remote_target_buffer: String,
    /// ~/.ssh/config 中的候选设备别名（只读展示，不连接）。
    pub remote_candidates: Vec<String>,
    /// 操作取消标志：后台线程完成时若已置位，结果被丢弃并提示取消。
    operation_cancel: Arc<AtomicBool>,
    /// 操作结果弹窗自动消失的 tick 截止（None = 常驻）。
    operation_dismiss_at: Option<u64>,
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
            scroll: std::collections::BTreeMap::new(),
            filter_mode: false,
            filter_buffer: String::new(),
            remote_target_mode: false,
            remote_selection: 0,
            remote_target_buffer: String::new(),
            remote_candidates: Vec::new(),
            operation_cancel: Arc::new(AtomicBool::new(false)),
            operation_dismiss_at: None,
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
        if self.remote_target_mode {
            self.handle_remote_target_key(key.code);
            return;
        }
        if self.filter_mode {
            self.handle_filter_key(key.code);
            return;
        }
        match key.code {
            KeyCode::Char('q') => self.should_quit = true,
            KeyCode::Esc => {
                // 已设置过滤时 Esc 先清除过滤，不退出。
                if !self.clear_filter() {
                    self.should_quit = true;
                }
            }
            KeyCode::Char('?') => self.show_help = !self.show_help,
            KeyCode::Char('r') => self.request_refresh(),
            KeyCode::Char('b') => self.request_operation(UiOperationKind::P2pBenchmark),
            KeyCode::Char('e') => self.request_operation(UiOperationKind::ReportExport),
            KeyCode::Char('s') => {
                // 进入远程设备选择模式：从候选列表中选一台，回车才扫描。
                self.remote_target_mode = true;
                self.remote_selection = 0;
                self.remote_target_buffer.clear();
            }
            KeyCode::Char('m') => self.report_format = self.report_format.next(),
            KeyCode::Char('/') => self.enter_filter(),
            KeyCode::Up | KeyCode::Char('k') => self.scroll_page(-1),
            KeyCode::Down | KeyCode::Char('j') => self.scroll_page(1),
            KeyCode::PageUp => self.scroll_page(-10),
            KeyCode::PageDown => self.scroll_page(10),
            KeyCode::Home => self.scroll_to(0),
            KeyCode::End => self.scroll_to(u16::MAX),
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

    fn handle_filter_key(&mut self, code: KeyCode) {
        match code {
            KeyCode::Esc | KeyCode::Enter => self.filter_mode = false,
            KeyCode::Backspace => {
                self.filter_buffer.pop();
            }
            KeyCode::Char(character) => self.filter_buffer.push(character),
            _ => {}
        }
        // 过滤条件变化时回到顶部，避免停留在被过滤掉的位置。
        self.scroll_to(0);
    }

    /// 远程设备选择模式：↑/↓ 或数字 1-9 移动光标，Enter 扫描选中设备，Esc 取消。
    fn handle_remote_target_key(&mut self, code: KeyCode) {
        let candidate_count = self.remote_candidates.len();
        match code {
            KeyCode::Enter => {
                if candidate_count == 0 {
                    self.remote_target_mode = false;
                    return;
                }
                let selected = self.remote_selection.min(candidate_count.saturating_sub(1));
                self.remote_target_buffer = self.remote_candidates[selected].clone();
                self.remote_target_mode = false;
                self.request_operation(UiOperationKind::RemoteScan);
            }
            KeyCode::Esc => {
                self.remote_target_mode = false;
                self.remote_target_buffer.clear();
            }
            KeyCode::Up | KeyCode::Char('k') => {
                self.remote_selection = self.remote_selection.saturating_sub(1);
            }
            KeyCode::Down | KeyCode::Char('j') => {
                if candidate_count > 0 {
                    self.remote_selection = (self.remote_selection + 1).min(candidate_count - 1);
                }
            }
            KeyCode::Char(digit) if digit.is_ascii_digit() && digit != '0' => {
                let index = (digit as usize) - ('1' as usize);
                if index < candidate_count {
                    self.remote_selection = index;
                }
            }
            _ => {}
        }
    }

    pub const fn remote_target_mode(&self) -> bool {
        self.remote_target_mode
    }

    /// 当前光标指向的候选索引。
    pub const fn remote_selection(&self) -> usize {
        self.remote_selection
    }

    /// 当前输入的目标别名（输入模式中实时生效）。
    pub fn remote_target(&self) -> &str {
        self.remote_target_buffer.trim()
    }

    /// 触发远程扫描时消费目标别名（清空缓冲）。
    pub fn take_remote_target(&mut self) -> String {
        let target = self.remote_target_buffer.trim().to_owned();
        self.remote_target_buffer.clear();
        target
    }

    /// 清除过滤并返回是否确实清除了（Esc 语义：有过滤先清过滤再退出）。
    fn clear_filter(&mut self) -> bool {
        if !self.filter_buffer.is_empty() || self.filter_mode {
            self.filter_buffer.clear();
            self.filter_mode = false;
            self.scroll_to(0);
            true
        } else {
            false
        }
    }

    /// 进入 `/` 过滤模式（仅日志/诊断页）。
    fn enter_filter(&mut self) {
        if matches!(self.page, UiPage::Logs | UiPage::Diagnosis) {
            self.filter_mode = true;
            self.filter_buffer.clear();
            self.scroll_to(0);
        }
    }

    pub const fn filter_mode(&self) -> bool {
        self.filter_mode
    }

    pub fn filter(&self) -> Option<&str> {
        if self.filter_mode || !self.filter_buffer.is_empty() {
            Some(self.filter_buffer.trim())
        } else {
            None
        }
    }

    /// 当前页滚动偏移。
    pub fn scroll_for(&self, page: UiPage) -> u16 {
        self.scroll.get(&page).copied().unwrap_or(0)
    }

    /// 当前页滚动偏移（供渲染使用）。
    pub fn current_scroll(&self) -> u16 {
        self.scroll_for(self.page)
    }

    fn scroll_page(&mut self, delta: i32) {
        let next = self.scroll_for(self.page) as i32 + delta;
        self.scroll_to(next.clamp(0, u16::MAX as i32) as u16);
    }

    fn scroll_to(&mut self, offset: u16) {
        if self.scroll_for(self.page) != offset {
            self.scroll.insert(self.page, offset);
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
            UiOperationState::Running(_) => match key {
                KeyCode::Esc => {
                    // 请求取消：后台线程完成时若标志已置位则丢弃结果。
                    self.operation_cancel.store(true, Ordering::SeqCst);
                    true
                }
                _ => true,
            },
            UiOperationState::Succeeded { .. } | UiOperationState::Failed { .. } => match key {
                KeyCode::Enter | KeyCode::Esc => {
                    self.operation = UiOperationState::Idle;
                    self.operation_dismiss_at = None;
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
        // 成功/失败弹窗 5 秒（20 tick × 250ms）后自动消失。
        self.operation_dismiss_at = Some(self.ui_tick_count.saturating_add(20));
    }

    pub fn refresh(&mut self) {
        self.refresh_count = self.refresh_count.saturating_add(1);
        self.refresh_requested = true;
    }

    pub fn tick(&mut self) {
        self.ui_tick_count = self.ui_tick_count.saturating_add(1);
        // 操作结果弹窗超时自动关闭。
        if let Some(deadline) = self.operation_dismiss_at {
            if self.ui_tick_count >= deadline {
                self.operation = UiOperationState::Idle;
                self.operation_dismiss_at = None;
            }
        }
    }

    pub fn request_refresh(&mut self) {
        self.refresh_count = self.refresh_count.saturating_add(1);
        self.refresh_requested = true;
    }

    /// 是否有待执行的刷新请求（不消费）。
    pub const fn refresh_requested(&self) -> bool {
        self.refresh_requested
    }

    /// 消费刷新请求（仅在能启动刷新线程时调用）。
    pub fn take_refresh_requested(&mut self) -> bool {
        let requested = self.refresh_requested;
        self.refresh_requested = false;
        requested
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

/// Full interactive entrypoint. 所有采集型操作（刷新/P2P/导出/远程扫描）都在后台
/// 线程执行（Esc 可取消），保持 Ratatui 事件循环响应。
pub fn run_with_actions<B, R, P, E, S>(
    terminal: &mut Terminal<B>,
    state: &mut AppState,
    refresh: R,
    benchmark: P,
    export: E,
    remote_scan: S,
) -> io::Result<()>
where
    B: Backend,
    R: Fn() -> RuntimeCollection + Send + Sync + 'static,
    P: Fn() -> (RuntimeCollection, Result<String, String>) + Send + Sync + 'static,
    E: Fn(RuntimeCollection, UiReportFormat) -> Result<String, String> + Send + Sync + 'static,
    S: Fn(&str) -> (crate::domain::RemoteScanSnapshot, Result<String, String>)
        + Send
        + Sync
        + 'static,
{
    let refresh = Arc::new(refresh);
    let benchmark = Arc::new(benchmark);
    let export = Arc::new(export);
    let remote_scan = Arc::new(remote_scan);
    let (sender, receiver) = mpsc::channel::<(
        UiOperationKind,
        Result<String, String>,
        Option<RuntimeCollection>,
        Option<crate::domain::RemoteScanSnapshot>,
    )>();
    let mut running_operation = None;
    let mut events = EventHandler::new(Duration::from_millis(250));

    while !state.should_quit() {
        while let Ok((kind, result, collection, remote)) = receiver.try_recv() {
            if let Some(collection) = collection {
                state.replace_collection(collection);
            }
            if let Some(remote) = remote {
                state.snapshot.remote = Some(remote);
            }
            state.complete_operation(kind, result);
            running_operation = None;
        }

        terminal.draw(|frame| ui::draw(frame, state))?;
        state.handle_event(events.next_event()?);

        let started = state.take_started_operation();
        if let Some(kind) = started.filter(|kind| running_operation != Some(*kind)) {
            running_operation = Some(kind);
            let sender = sender.clone();
            // 新操作开始前重置取消标志；后台线程完成后检查。
            state.operation_cancel.store(false, Ordering::SeqCst);
            match kind {
                UiOperationKind::P2pBenchmark => {
                    let benchmark = Arc::clone(&benchmark);
                    let cancel = Arc::clone(&state.operation_cancel);
                    thread::spawn(move || {
                        let (collection, result) = benchmark();
                        let result = cancel_result(&cancel, result);
                        let _ = sender.send((kind, result, Some(collection), None));
                    });
                }
                UiOperationKind::ReportExport => {
                    let export = Arc::clone(&export);
                    let cancel = Arc::clone(&state.operation_cancel);
                    let collection = state.collection();
                    let format = state.report_format;
                    thread::spawn(move || {
                        let result = match export(collection, format) {
                            Ok(path) => Ok(format!("报告已写入：{path}")),
                            Err(message) => Err(message),
                        };
                        let result = cancel_result(&cancel, result);
                        let _ = sender.send((kind, result, None, None));
                    });
                }
                UiOperationKind::RemoteScan => {
                    let remote_scan = Arc::clone(&remote_scan);
                    let cancel = Arc::clone(&state.operation_cancel);
                    let target = state.take_remote_target();
                    thread::spawn(move || {
                        let (snapshot, result) = remote_scan(&target);
                        let result = cancel_result(&cancel, result);
                        let _ = sender.send((kind, result, None, Some(snapshot)));
                    });
                }
                // 刷新（r 键）不经过确认弹窗，由主循环独立启动（见下方 refresh_requested 分支）。
                UiOperationKind::Refresh => unreachable!("刷新不走 started 操作路径"),
            }
        }

        // 刷新请求（r 键）：无运行中操作时在后台线程执行，UI 不冻结。
        if state.refresh_requested() && running_operation.is_none() {
            state.take_refresh_requested();
            let kind = UiOperationKind::Refresh;
            running_operation = Some(kind);
            state.operation = UiOperationState::Running(kind);
            state.operation_cancel.store(false, Ordering::SeqCst);
            let refresh = Arc::clone(&refresh);
            let cancel = Arc::clone(&state.operation_cancel);
            let sender = sender.clone();
            thread::spawn(move || {
                let collection = refresh();
                let result = if cancel.load(Ordering::SeqCst) {
                    Err("操作已取消".to_owned())
                } else {
                    Ok("采集刷新完成".to_owned())
                };
                let _ = sender.send((kind, result, Some(collection), None));
            });
        }
    }
    Ok(())
}

/// 操作完成时若取消标志已置位，将结果替换为"已取消"。
fn cancel_result(cancel: &AtomicBool, result: Result<String, String>) -> Result<String, String> {
    if cancel.load(Ordering::SeqCst) {
        Err("操作已取消".to_owned())
    } else {
        result
    }
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
    fn scroll_keys_adjust_per_page_offset() {
        let mut state = AppState::new(demo::snapshot());
        state.handle_key(key(KeyCode::Down));
        assert_eq!(state.current_scroll(), 1);
        state.handle_key(key(KeyCode::PageDown));
        assert_eq!(state.current_scroll(), 11);
        state.handle_key(key(KeyCode::Up));
        assert_eq!(state.current_scroll(), 10);
        state.handle_key(key(KeyCode::Home));
        assert_eq!(state.current_scroll(), 0);

        // 切页保留各自滚动偏移。
        state.handle_key(key(KeyCode::Char('2')));
        state.handle_key(key(KeyCode::Down));
        assert_eq!(state.scroll_for(UiPage::Gpu), 1);
        state.handle_key(key(KeyCode::Char('6')));
        assert_eq!(state.current_scroll(), 0);
    }

    #[test]
    fn filter_mode_accumulates_chars_and_esc_clears_before_quitting() {
        let mut state = AppState::new(demo::snapshot());
        state.handle_key(key(KeyCode::Char('6'))); // 日志页
        state.handle_key(key(KeyCode::Char('/')));
        assert!(state.filter_mode());
        state.handle_key(key(KeyCode::Char('X')));
        state.handle_key(key(KeyCode::Char('i')));
        assert_eq!(state.filter(), Some("Xi"));

        // 过滤模式内 Esc 退出输入模式但保留过滤。
        state.handle_key(key(KeyCode::Esc));
        assert!(!state.filter_mode());
        assert_eq!(state.filter(), Some("Xi"));
        assert!(!state.should_quit());

        // 非过滤模式下 Esc 先清除过滤，再按一次才退出。
        state.handle_key(key(KeyCode::Esc));
        assert_eq!(state.filter(), None);
        assert!(!state.should_quit());
        state.handle_key(key(KeyCode::Esc));
        assert!(state.should_quit());
    }

    #[test]
    fn slash_filter_only_activates_on_log_and_diagnosis_pages() {
        let mut state = AppState::new(demo::snapshot());
        state.handle_key(key(KeyCode::Char('1'))); // 总览页
        state.handle_key(key(KeyCode::Char('/')));
        assert!(!state.filter_mode());
        state.handle_key(key(KeyCode::Char('6'))); // 日志页
        state.handle_key(key(KeyCode::Char('/')));
        assert!(state.filter_mode());
    }

    #[test]
    fn remote_scan_s_key_selects_candidate_and_enter_confirms() {
        let mut state = AppState::new(demo::snapshot());
        state.remote_candidates = vec![
            "host-a".to_owned(),
            "wfk8smaster3".to_owned(),
            "host-c".to_owned(),
        ];
        // s 键：进入候选选择模式，不直接触发扫描
        state.handle_key(key(KeyCode::Char('s')));
        assert!(state.remote_target_mode());
        assert_eq!(state.remote_selection(), 0);
        assert_eq!(state.operation, UiOperationState::Idle);
        // ↓ 移动光标到第 2 台
        state.handle_key(key(KeyCode::Down));
        assert_eq!(state.remote_selection(), 1);
        // 数字 3 快捷选择第 3 台
        state.handle_key(key(KeyCode::Char('3')));
        assert_eq!(state.remote_selection(), 2);
        // Enter：选中并请求扫描
        state.handle_key(key(KeyCode::Enter));
        assert!(!state.remote_target_mode());
        assert_eq!(
            state.operation,
            UiOperationState::Confirm(UiOperationKind::RemoteScan)
        );
        // take_remote_target 消费选中的别名
        let target = state.take_remote_target();
        assert_eq!(target, "host-c");
        assert_eq!(state.remote_target(), "");
    }

    #[test]
    fn remote_selection_clamps_at_list_bounds() {
        let mut state = AppState::new(demo::snapshot());
        state.remote_candidates = vec!["only".to_owned()];
        state.handle_key(key(KeyCode::Char('s')));
        // 列表只有 1 台：↓ 与数字 9 都应停在边界
        state.handle_key(key(KeyCode::Down));
        assert_eq!(state.remote_selection(), 0);
        state.handle_key(key(KeyCode::Char('9')));
        assert_eq!(state.remote_selection(), 0);
        // 数字 3 超出候选数 → 光标不变
        state.handle_key(key(KeyCode::Char('3')));
        assert_eq!(state.remote_selection(), 0);
    }

    #[test]
    fn remote_scan_esc_cancels_selection_without_scanning() {
        let mut state = AppState::new(demo::snapshot());
        state.remote_candidates = vec!["wfk8smaster3".to_owned()];
        state.handle_key(key(KeyCode::Char('s')));
        state.handle_key(key(KeyCode::Down));
        state.handle_key(key(KeyCode::Esc));
        assert!(!state.remote_target_mode());
        assert_eq!(state.remote_target(), "");
        assert_eq!(state.operation, UiOperationState::Idle);
        assert!(!state.should_quit());
    }

    #[test]
    fn operation_result_dialog_auto_dismisses_after_ticks() {
        let mut state = AppState::new(demo::snapshot());
        state.complete_operation(UiOperationKind::Refresh, Ok("采集刷新完成".to_owned()));
        assert!(matches!(
            state.operation,
            UiOperationState::Succeeded { .. }
        ));
        // 5 秒（20 tick）内保持显示，可按键立即关闭
        for _ in 0..10 {
            state.tick();
        }
        assert!(
            matches!(state.operation, UiOperationState::Succeeded { .. }),
            "10 tick 内应仍显示"
        );
        state.tick(); // 从 complete 时 ui_tick_count=0 起算，第 21 个 tick 后超时
        state.tick();
        for _ in 0..20 {
            state.tick();
        }
        assert_eq!(state.operation, UiOperationState::Idle, "超时后应自动关闭");
        // 按键关闭后不再受 tick 影响
        state.complete_operation(UiOperationKind::Refresh, Err("已取消".to_owned()));
        state.handle_key(key(KeyCode::Esc));
        assert_eq!(state.operation, UiOperationState::Idle);
        for _ in 0..30 {
            state.tick();
        }
        assert_eq!(state.operation, UiOperationState::Idle);
    }

    #[test]
    fn refresh_request_flag_can_be_taken_once() {
        let mut state = AppState::new(demo::snapshot());
        assert!(!state.refresh_requested());
        state.request_refresh();
        assert!(state.refresh_requested());
        assert!(state.take_refresh_requested());
        assert!(!state.refresh_requested());
        assert!(!state.take_refresh_requested());
    }

    #[test]
    fn running_operation_esc_requests_cancel_not_quit() {
        let mut state = AppState::new(demo::snapshot());
        state.request_operation(UiOperationKind::P2pBenchmark);
        state.handle_key(key(KeyCode::Enter)); // 确认 → Running
        assert!(matches!(
            state.operation,
            UiOperationState::Running(UiOperationKind::P2pBenchmark)
        ));
        assert!(!state
            .operation_cancel
            .load(std::sync::atomic::Ordering::SeqCst));

        // Running 态 Esc：设置取消标志，不退出。
        state.handle_key(key(KeyCode::Esc));
        assert!(state
            .operation_cancel
            .load(std::sync::atomic::Ordering::SeqCst));
        assert!(!state.should_quit());
        // 其他键在 Running 态被吞掉，不改变状态。
        state.handle_key(key(KeyCode::Char('r')));
        assert!(matches!(
            state.operation,
            UiOperationState::Running(UiOperationKind::P2pBenchmark)
        ));
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
