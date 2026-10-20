//! Application state and keyboard routing.
//!
//! The UI task owns all of this state. It never performs network or filesystem
//! I/O directly: every such request becomes a [`Cmd`] on the MPSC channel to the
//! network task, and results come back as [`Evt`].

use std::collections::VecDeque;
use std::path::PathBuf;

use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use ratatui::widgets::TableState;
use tokio::sync::{mpsc, oneshot};

use crate::event::{
    Cmd, Endpoint, Evt, FsEntry, LogLevel, PaneKind, local_join, local_normalize, local_parent,
    remote_join, remote_normalize, remote_parent,
};
use crate::form::{ConfigFill, ConnectForm, DEFAULT_KEY_PATH, Field, TextField};
use crate::keys;
use crate::sshconfig::SshConfig;

/// Terminal history the parser is built with. It is a *capacity*: how much of
/// it is filled is asked of the screen, never assumed (see `AGENTS.md`).
const SCROLLBACK_LINES: usize = 2000;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Screen {
    Connect,
    Main,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tab {
    Terminal,
    Sftp,
    Logs,
}

impl Tab {
    pub fn index(self) -> usize {
        match self {
            Self::Terminal => 0,
            Self::Sftp => 1,
            Self::Logs => 2,
        }
    }

    pub fn next(self) -> Self {
        match self {
            Self::Terminal => Self::Sftp,
            Self::Sftp => Self::Logs,
            Self::Logs => Self::Terminal,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Focus {
    Local,
    Remote,
}

impl Focus {
    pub fn kind(self) -> PaneKind {
        match self {
            Self::Local => PaneKind::Local,
            Self::Remote => PaneKind::Remote,
        }
    }

    pub fn toggled(self) -> Self {
        match self {
            Self::Local => Self::Remote,
            Self::Remote => Self::Local,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Local => "Left",
            Self::Remote => "Right",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SftpState {
    NotInitialized,
    Initializing,
    Ready,
    Failed(String),
}

impl SftpState {
    pub fn is_ready(&self) -> bool {
        matches!(self, Self::Ready)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SortKey {
    Name,
    Size,
}

impl SortKey {
    pub fn label(self) -> &'static str {
        match self {
            Self::Name => "name",
            Self::Size => "size",
        }
    }

    fn next(self) -> Self {
        match self {
            Self::Name => Self::Size,
            Self::Size => Self::Name,
        }
    }
}

/// Case-insensitive name order with an exact-case tie-break.
fn name_order(left: &FsEntry, right: &FsEntry) -> std::cmp::Ordering {
    left.name
        .to_lowercase()
        .cmp(&right.name.to_lowercase())
        .then_with(|| left.name.cmp(&right.name))
}

pub struct Pane {
    pub cwd: String,
    /// The complete listing. [`Pane::view`] holds the indices that survive the
    /// current filter, in display order.
    pub entries: Vec<FsEntry>,
    view: Vec<usize>,
    pub state: TableState,
    sort: SortKey,
    descending: bool,
    filter: String,
    /// `Some` while the inline search field is open. The field's text is kept
    /// in sync with `filter` on every edit.
    search: Option<TextField>,
}

impl Pane {
    pub fn new(cwd: impl Into<String>) -> Self {
        Self {
            cwd: cwd.into(),
            entries: Vec::new(),
            view: Vec::new(),
            state: TableState::default(),
            sort: SortKey::Name,
            descending: false,
            filter: String::new(),
            search: None,
        }
    }

    pub fn visible(&self) -> impl Iterator<Item = &FsEntry> + '_ {
        self.view
            .iter()
            .filter_map(|&index| self.entries.get(index))
    }

    pub fn selected(&self) -> Option<&FsEntry> {
        self.state
            .selected()
            .and_then(|index| self.view.get(index))
            .and_then(|&index| self.entries.get(index))
    }

    pub fn selected_name(&self) -> Option<String> {
        self.selected().map(|e| e.name.clone())
    }

    pub fn sort_key(&self) -> SortKey {
        self.sort
    }

    pub fn sort_descending(&self) -> bool {
        self.descending
    }

    pub fn filter(&self) -> &str {
        &self.filter
    }

    pub fn is_searching(&self) -> bool {
        self.search.is_some()
    }

    pub fn search_field(&self) -> Option<&TextField> {
        self.search.as_ref()
    }

    /// Replace the listing, keeping the selection on the same name when
    /// possible. This makes refresh/rename feel stable. A new directory starts
    /// with a blank filter, while refreshing the same one keeps it.
    pub fn set_entries(&mut self, cwd: String, entries: Vec<FsEntry>) {
        let previous = self.selected_name();
        let changed_dir = cwd != self.cwd;
        self.cwd = cwd;
        self.entries = entries;
        if changed_dir {
            self.filter.clear();
            self.search = None;
        }
        self.rebuild(previous);
    }

    /// Recompute [`Pane::view`] from the current filter and sort, then keep the
    /// selection on `select` (a filename) when it is still visible.
    fn rebuild(&mut self, select: Option<String>) {
        let needle = self.filter.to_lowercase();
        self.view = self
            .entries
            .iter()
            .enumerate()
            .filter(|(_, entry)| needle.is_empty() || entry.name.to_lowercase().contains(&needle))
            .map(|(index, _)| index)
            .collect();

        let sort = self.sort;
        let descending = self.descending;
        self.view.sort_by(|&a, &b| {
            let left = &self.entries[a];
            let right = &self.entries[b];
            // Directories stay grouped first in either direction: their size
            // is not meaningful, and this matches the order the network layer
            // already produces.
            if left.is_dir != right.is_dir {
                return right.is_dir.cmp(&left.is_dir);
            }
            match sort {
                SortKey::Name => {
                    let order = name_order(left, right);
                    if descending { order.reverse() } else { order }
                }
                SortKey::Size if left.is_dir => name_order(left, right),
                SortKey::Size => {
                    let order = left.size.cmp(&right.size);
                    let order = if descending { order.reverse() } else { order };
                    order.then_with(|| name_order(left, right))
                }
            }
        });

        let index = select
            .and_then(|name| {
                self.view
                    .iter()
                    .position(|&index| self.entries[index].name == name)
            })
            .or(if self.view.is_empty() { None } else { Some(0) });
        self.state.select(index);
        *self.state.offset_mut() = 0;
    }

    pub fn cycle_sort(&mut self) {
        let previous = self.selected_name();
        self.sort = self.sort.next();
        self.descending = false;
        self.rebuild(previous);
    }

    pub fn toggle_sort_direction(&mut self) {
        let previous = self.selected_name();
        self.descending = !self.descending;
        self.rebuild(previous);
    }

    pub fn begin_search(&mut self) {
        self.search = Some(TextField::new(self.filter.clone()));
    }

    /// Copy the field into the filter and re-derive the visible rows.
    fn sync_filter(&mut self) {
        let previous = self.selected_name();
        if let Some(field) = self.search.as_ref() {
            self.filter = field.value.clone();
        }
        self.rebuild(previous);
    }

    pub fn search_insert(&mut self, c: char) {
        if let Some(field) = self.search.as_mut() {
            field.insert(c);
        }
        self.sync_filter();
    }

    pub fn search_backspace(&mut self) {
        if let Some(field) = self.search.as_mut() {
            field.backspace();
        }
        self.sync_filter();
    }

    pub fn search_delete(&mut self) {
        if let Some(field) = self.search.as_mut() {
            field.delete();
        }
        self.sync_filter();
    }

    pub fn search_clear(&mut self) {
        if let Some(field) = self.search.as_mut() {
            *field = TextField::default();
        }
        self.sync_filter();
    }

    pub fn search_left(&mut self) {
        if let Some(field) = self.search.as_mut() {
            field.left();
        }
    }

    pub fn search_right(&mut self) {
        if let Some(field) = self.search.as_mut() {
            field.right();
        }
    }

    pub fn search_home(&mut self) {
        if let Some(field) = self.search.as_mut() {
            field.home();
        }
    }

    pub fn search_end(&mut self) {
        if let Some(field) = self.search.as_mut() {
            field.end();
        }
    }

    /// Close the field but keep the filter applied.
    pub fn search_commit(&mut self) {
        self.search = None;
    }

    /// Close the field and show the whole directory again.
    pub fn search_cancel(&mut self) {
        let previous = self.selected_name();
        self.search = None;
        self.filter.clear();
        self.rebuild(previous);
    }

    pub fn move_selection(&mut self, delta: isize) {
        if self.view.is_empty() {
            self.state.select(None);
            return;
        }
        let current = self.state.selected().unwrap_or(0) as isize;
        let last = self.view.len() as isize - 1;
        self.state
            .select(Some((current + delta).clamp(0, last) as usize));
    }

    pub fn select_first(&mut self) {
        if !self.view.is_empty() {
            self.state.select(Some(0));
            *self.state.offset_mut() = 0;
        }
    }

    pub fn select_last(&mut self) {
        if !self.view.is_empty() {
            self.state.select(Some(self.view.len() - 1));
        }
    }
}

pub struct LogLine {
    pub ts: String,
    pub level: LogLevel,
    pub text: String,
}

#[derive(Default)]
pub struct LogBuffer {
    pub lines: VecDeque<LogLine>,
    /// How many lines we are scrolled up from the bottom (0 = follow).
    pub scroll: usize,
}

impl LogBuffer {
    pub fn push(&mut self, level: LogLevel, text: String) {
        self.lines.push_back(LogLine {
            ts: chrono::Local::now().format("%H:%M:%S").to_string(),
            level,
            text,
        });
        while self.lines.len() > 2000 {
            self.lines.pop_front();
        }
    }
}

pub struct HostKeyModal {
    pub host: String,
    pub port: u16,
    pub key_type: String,
    pub fingerprint: String,
    pub answer: Option<oneshot::Sender<bool>>,
}

pub struct InputModal {
    pub title: String,
    pub label: String,
    pub field: TextField,
    pub error: Option<String>,
    pub action: InputAction,
}

pub enum InputAction {
    Rename {
        pane: PaneKind,
        from: String,
    },
    Mkdir {
        pane: PaneKind,
        parent: String,
    },
    /// `base` is the pane's cwd when the dialog opened; a relative path is
    /// resolved against it. Absolute paths replace it.
    ChangeDir {
        pane: PaneKind,
        base: String,
    },
}

pub struct ConfirmModal {
    pub title: String,
    pub message: String,
    pub action: ConfirmAction,
}

pub enum ConfirmAction {
    Delete {
        pane: PaneKind,
        path: String,
        is_dir: bool,
    },
}

pub enum Modal {
    HostKey(HostKeyModal),
    Input(InputModal),
    Confirm(ConfirmModal),
}

#[derive(Debug, Clone)]
pub struct TransferStatus {
    pub id: u64,
    pub files_done: usize,
    pub files_total: usize,
    pub bytes_done: u64,
    pub bytes_total: u64,
}

impl TransferStatus {
    pub fn percent(&self) -> u16 {
        if self.bytes_total == 0 {
            if self.files_total == 0 {
                return 100;
            }
            return ((self.files_done * 100) / self.files_total) as u16;
        }
        ((self.bytes_done.min(self.bytes_total) * 100) / self.bytes_total) as u16
    }

    pub fn summary(&self) -> String {
        format!(
            "copy {:>3}%  {}/{} files  {}/{}",
            self.percent(),
            self.files_done,
            self.files_total,
            crate::event::human_size(self.bytes_done),
            crate::event::human_size(self.bytes_total),
        )
    }
}

pub struct App {
    pub screen: Screen,
    pub tab: Tab,
    pub focus: Focus,
    pub parser: vt100::Parser,
    pub local: Pane,
    pub remote: Pane,
    pub logs: LogBuffer,
    pub form: ConnectForm,
    pub modal: Option<Modal>,
    pub sftp: SftpState,
    pub status: String,
    pub transfer: Option<TransferStatus>,
    pub pty_cols: u16,
    pub pty_rows: u16,
    pub should_quit: bool,
    pub connected: bool,
    pub host_label: String,
    pub next_transfer_id: u64,
    /// Phase of the locally drawn SSH cursor: `true` is the "on" half of the
    /// blink. The UI loop flips it every 500 ms; keys and PTY output turn it
    /// back on so typing never waits for the next phase.
    pub blink_on: bool,
    /// Parsed once at startup (`main::run`); lookups are pure.
    ssh_config: SshConfig,
    tx: mpsc::Sender<Cmd>,
}

impl App {
    pub fn new(tx: mpsc::Sender<Cmd>, prefill: Option<String>) -> Self {
        let mut form = ConnectForm::default();
        if let Some(target) = prefill {
            form.apply_target(&target);
        }
        let cwd = std::env::current_dir()
            .map(|p| p.display().to_string())
            .unwrap_or_else(|_| ".".to_string());

        let mut app = Self {
            screen: Screen::Connect,
            tab: Tab::Terminal,
            focus: Focus::Local,
            parser: vt100::Parser::new(24, 80, SCROLLBACK_LINES),
            local: Pane::new(cwd),
            remote: Pane::new("/"),
            logs: LogBuffer::default(),
            form,
            modal: None,
            sftp: SftpState::NotInitialized,
            status: String::new(),
            transfer: None,
            pty_cols: 80,
            pty_rows: 24,
            should_quit: false,
            connected: false,
            host_label: "sshftp-tui".to_string(),
            next_transfer_id: 1,
            blink_on: true,
            ssh_config: SshConfig::default(),
            tx,
        };
        app.log(
            LogLevel::Info,
            "sshftp-tui ready: fill in the connection details and press Enter",
        );
        app
    }

    pub fn log(&mut self, level: LogLevel, message: impl Into<String>) {
        self.logs.push(level, message.into());
    }

    pub fn set_status(&mut self, status: impl Into<String>) {
        self.status = status.into();
    }

    /// One half-period of the SSH cursor blink, driven by the UI loop.
    pub fn blink_tick(&mut self) {
        self.blink_on = !self.blink_on;
    }

    async fn send(&self, cmd: Cmd) {
        let _ = self.tx.send(cmd).await;
    }

    pub fn cycle_tab(&mut self) {
        self.tab = self.tab.next();
    }

    /// Called with the *full* terminal size. The PTY occupies the terminal
    /// tab's inner area: window minus 3 lines of tabs, 1 line of footer and
    /// the 2 border lines.
    pub fn on_resize(&mut self, width: u16, height: u16) {
        let cols = width.saturating_sub(2).max(20);
        let rows = height.saturating_sub(6).max(5);
        self.fit_pty(cols, rows);
    }

    /// Resize the vt100 grid and inform the remote shell (SIGWINCH).
    pub fn fit_pty(&mut self, cols: u16, rows: u16) {
        if cols == 0 || rows == 0 {
            return;
        }
        self.pty_cols = cols;
        self.pty_rows = rows;
        if self.parser.screen().size() != (rows, cols) {
            self.parser.screen_mut().set_size(rows, cols);
            let _ = self.tx.try_send(Cmd::Resize { cols, rows });
        }
    }

    pub async fn on_key(&mut self, key: KeyEvent) {
        if key.kind != KeyEventKind::Press {
            return;
        }
        // Keep the caret solid while the user is typing.
        self.blink_on = true;
        if self.modal.is_some() {
            self.on_modal_key(key).await;
            return;
        }
        if key.modifiers.contains(KeyModifiers::CONTROL) && matches!(key.code, KeyCode::Char('t')) {
            self.cycle_tab();
            return;
        }
        if key.code == KeyCode::F(10) {
            self.should_quit = true;
            return;
        }
        match self.screen {
            Screen::Connect => self.on_connect_key(key).await,
            Screen::Main => match self.tab {
                Tab::Terminal => self.on_terminal_key(key).await,
                Tab::Sftp => self.on_sftp_key(key).await,
                Tab::Logs => self.on_logs_key(key),
            },
        }
    }

    async fn on_connect_key(&mut self, key: KeyEvent) {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        let alt = key.modifiers.contains(KeyModifiers::ALT);
        match key.code {
            KeyCode::Esc => self.should_quit = true,
            KeyCode::Enter if !self.form.connecting => self.submit_connect().await,
            KeyCode::Tab => {
                self.leave_host_field();
                self.form.focus_next(false);
            }
            KeyCode::BackTab | KeyCode::Up => {
                self.leave_host_field();
                self.form.focus_next(true);
            }
            KeyCode::Down => {
                self.leave_host_field();
                self.form.focus_next(false);
            }
            KeyCode::Left if self.form.focus == Field::Auth && !self.form.connecting => {
                self.form.toggle_auth();
            }
            KeyCode::Right if self.form.focus == Field::Auth && !self.form.connecting => {
                self.form.toggle_auth();
            }
            KeyCode::Char(c) if !ctrl && !alt && !self.form.connecting => {
                let focus = self.form.focus;
                if let Some(field) = self.form.field_mut(focus) {
                    field.insert(c);
                }
                self.form.error = None;
            }
            KeyCode::Backspace if !self.form.connecting => {
                let focus = self.form.focus;
                if let Some(field) = self.form.field_mut(focus) {
                    field.backspace();
                }
            }
            KeyCode::Delete if !self.form.connecting => {
                let focus = self.form.focus;
                if let Some(field) = self.form.field_mut(focus) {
                    field.delete();
                }
            }
            KeyCode::Home if !self.form.connecting => {
                let focus = self.form.focus;
                if let Some(field) = self.form.field_mut(focus) {
                    field.home();
                }
            }
            KeyCode::End if !self.form.connecting => {
                let focus = self.form.focus;
                if let Some(field) = self.form.field_mut(focus) {
                    field.end();
                }
            }
            _ => {}
        }
    }

    /// The session ended — remote logout, exit status, dropped transport. Drop
    /// back to the login screen so the user can open a new session (or quit)
    /// instead of looking at a shell that can no longer answer.
    fn back_to_login(&mut self, reason: &str) {
        self.screen = Screen::Connect;
        self.connected = false;
        // The chosen policy is a blank form. `ConnectForm::default()` restores
        // only the transport defaults (port and key path), never the local
        // account's username.
        self.form = ConnectForm::default();
        // `draw_connect` has no status line, so the reason goes where the form
        // already reports problems: on the line below the fields.
        self.form.error = Some(format!("{reason} — sign in again"));
        self.modal = None;
        // Everything below belongs to the SSH session that just died.
        self.sftp = SftpState::NotInitialized;
        self.remote = Pane::new("/");
        self.transfer = None;
        self.host_label = "sshftp-tui".to_string();
    }

    /// Hands the parsed `~/.ssh/config` to the app: `main::run` calls this once,
    /// before the first frame, and the tests call it with a parsed fixture.
    pub fn set_ssh_config(&mut self, config: SshConfig) {
        self.ssh_config = config;
        // A target on the command line may already name an alias.
        if let Err(error) = self.apply_ssh_config() {
            self.form.error = Some(error);
        }
    }

    /// Fills the form from the parsed config. Only fields still holding their
    /// defaults are replaced, and the Host field becomes the resolved
    /// `HostName`, so what the form shows is exactly what will be dialled.
    ///
    /// `ProxyJump`/`ProxyCommand` are refused rather than ignored: this client
    /// cannot reach a host through a bastion, and pretending otherwise would
    /// hide that the connection is a different one.
    fn apply_ssh_config(&mut self) -> Result<(), String> {
        let typed = self.form.host.trimmed().to_string();
        if typed.is_empty() {
            return Ok(());
        }

        let Some(resolved) = self.ssh_config.lookup(&typed) else {
            // Keep the note when the Host field already holds the hostname we
            // resolved; clear it when the operator typed something else.
            if !self
                .form
                .config
                .as_ref()
                .is_some_and(|fill| fill.hostname == typed)
            {
                self.form.config = None;
            }
            return Ok(());
        };

        if let Some(proxy) = &resolved.proxy {
            self.form.config = None;
            return Err(format!(
                "{proxy} is set for {typed} in ~/.ssh/config; jump hosts are not supported, connect directly or use ssh"
            ));
        }

        if resolved.hostname != typed {
            self.form.host = TextField::new(&resolved.hostname);
        }
        if self.form.username.trimmed().is_empty() {
            if let Some(user) = &resolved.user {
                self.form.username = TextField::new(user);
            }
        }
        if self.form.port.trimmed() == "22" {
            if let Some(port) = resolved.port {
                self.form.port = TextField::new(port.to_string());
            }
        }
        if self.form.key_path.trimmed() == DEFAULT_KEY_PATH {
            if let Some(key) = &resolved.identity_file {
                self.form.key_path = TextField::new(key);
            }
        }

        self.form.config = Some(ConfigFill {
            source: self.ssh_config.label().to_string(),
            endpoint: self.form.endpoint(),
            alias: resolved.alias,
            hostname: resolved.hostname,
        });
        Ok(())
    }

    /// Resolve the alias as soon as the operator leaves the Host field, so the
    /// prefilled values are visible before Enter is pressed.
    fn leave_host_field(&mut self) {
        if self.form.focus != Field::Host {
            return;
        }
        if let Err(error) = self.apply_ssh_config() {
            self.form.error = Some(error);
        }
    }

    async fn submit_connect(&mut self) {
        // A `Host` alias may be the only thing in the form; resolve it before
        // validation so the config can supply the username and port.
        if let Err(error) = self.apply_ssh_config() {
            self.form.error = Some(error);
            self.form.connecting = false;
            return;
        }
        match self.form.build_params(self.pty_cols, self.pty_rows) {
            Ok(params) => {
                // A fresh session starts on a blank grid: neither the previous
                // session's screen nor its history may show through, and the
                // view must not open scrolled back.
                self.parser = vt100::Parser::new(self.pty_rows, self.pty_cols, SCROLLBACK_LINES);
                self.form.error = None;
                self.form.connecting = true;
                self.log(
                    LogLevel::Info,
                    format!(
                        "connecting to {}@{}:{}",
                        params.username, params.host, params.port
                    ),
                );
                self.set_status(format!("Connecting to {}:{}…", params.host, params.port));
                self.send(Cmd::Connect(params)).await;
            }
            Err(error) => {
                self.form.error = Some(error);
                self.form.connecting = false;
            }
        }
    }

    async fn on_terminal_key(&mut self, key: KeyEvent) {
        // Shift+PgUp/PgDn walk the parser's history and Shift+Home/End jump to
        // its ends. They are intercepted here so the unmodified keys can keep
        // reaching the remote shell, where `less`, `vim` and `tmux` want them.
        if key.modifiers.contains(KeyModifiers::SHIFT) {
            let page = self.pty_rows.saturating_sub(1).max(1) as usize;
            let offset = self.parser.screen().scrollback();
            let target = match key.code {
                KeyCode::PageUp => Some(offset + page),
                KeyCode::PageDown => Some(offset.saturating_sub(page)),
                // `set_scrollback` clamps to the history that actually exists.
                KeyCode::Home => Some(usize::MAX),
                KeyCode::End => Some(0),
                _ => None,
            };
            if let Some(target) = target {
                self.parser.screen_mut().set_scrollback(target);
                return;
            }
        }
        if let Some(bytes) = keys::encode(key) {
            self.send(Cmd::PtyInput(bytes)).await;
        }
    }

    async fn on_sftp_key(&mut self, key: KeyEvent) {
        // The inline find field owns every key while it is open; otherwise its
        // characters would trigger the pane shortcuts.
        if self.pane(self.focus).is_searching() {
            self.on_search_key(key).await;
            return;
        }

        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        let alt = key.modifiers.contains(KeyModifiers::ALT);
        match key.code {
            KeyCode::F(3) => self.init_sftp().await,
            KeyCode::Tab | KeyCode::BackTab => self.focus = self.focus.toggled(),
            KeyCode::Up => self.pane_mut(self.focus).move_selection(-1),
            KeyCode::Down => self.pane_mut(self.focus).move_selection(1),
            KeyCode::PageUp => self.pane_mut(self.focus).move_selection(-10),
            KeyCode::PageDown => self.pane_mut(self.focus).move_selection(10),
            KeyCode::Home => self.pane_mut(self.focus).select_first(),
            KeyCode::End => self.pane_mut(self.focus).select_last(),
            KeyCode::Enter => self.open_selected().await,
            KeyCode::Backspace | KeyCode::Left => self.go_up().await,
            KeyCode::F(4) => self.refresh_focused().await,
            KeyCode::F(5) => self.copy_selected().await,
            KeyCode::F(6) => self.open_rename_modal(),
            KeyCode::F(7) => self.open_mkdir_modal(),
            KeyCode::F(8) => self.open_delete_modal(),
            KeyCode::F(9) => self.open_chdir_modal(),
            KeyCode::Char('/') if !ctrl && !alt => {
                if self.focus == Focus::Remote && !self.sftp.is_ready() {
                    self.set_status("Press [F3] to initialize the SFTP session first");
                } else {
                    self.pane_mut(self.focus).begin_search();
                    self.set_status("Type to filter; Enter keeps it, Esc clears it");
                }
            }
            KeyCode::Char('s') if !ctrl && !alt => self.cycle_sort(),
            KeyCode::Char('S') if !ctrl && !alt => self.reverse_sort(),
            _ => {}
        }
    }

    /// Keys while the inline find field is open. Only editing keys and
    /// navigation are accepted; `Enter` keeps the filter and opens the
    /// selection, `Esc` drops back to the full directory.
    async fn on_search_key(&mut self, key: KeyEvent) {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        let alt = key.modifiers.contains(KeyModifiers::ALT);
        match key.code {
            KeyCode::Esc => self.pane_mut(self.focus).search_cancel(),
            KeyCode::Enter => {
                self.pane_mut(self.focus).search_commit();
                self.open_selected().await;
            }
            KeyCode::Tab | KeyCode::BackTab => {
                self.pane_mut(self.focus).search_commit();
                self.focus = self.focus.toggled();
            }
            KeyCode::Up => self.pane_mut(self.focus).move_selection(-1),
            KeyCode::Down => self.pane_mut(self.focus).move_selection(1),
            KeyCode::PageUp => self.pane_mut(self.focus).move_selection(-10),
            KeyCode::PageDown => self.pane_mut(self.focus).move_selection(10),
            KeyCode::Char('u') if ctrl => self.pane_mut(self.focus).search_clear(),
            KeyCode::Backspace => self.pane_mut(self.focus).search_backspace(),
            KeyCode::Delete => self.pane_mut(self.focus).search_delete(),
            KeyCode::Left => self.pane_mut(self.focus).search_left(),
            KeyCode::Right => self.pane_mut(self.focus).search_right(),
            KeyCode::Home => self.pane_mut(self.focus).search_home(),
            KeyCode::End => self.pane_mut(self.focus).search_end(),
            KeyCode::Char(c) if !ctrl && !alt => self.pane_mut(self.focus).search_insert(c),
            _ => {}
        }
    }

    fn cycle_sort(&mut self) {
        let (key, descending) = {
            let pane = self.pane_mut(self.focus);
            pane.cycle_sort();
            (pane.sort_key().label(), pane.sort_descending())
        };
        self.set_status(format!(
            "Sort: {key} {}",
            if descending {
                "descending"
            } else {
                "ascending"
            }
        ));
    }

    fn reverse_sort(&mut self) {
        let (key, descending) = {
            let pane = self.pane_mut(self.focus);
            pane.toggle_sort_direction();
            (pane.sort_key().label(), pane.sort_descending())
        };
        self.set_status(format!(
            "Sort: {key} {}",
            if descending {
                "descending"
            } else {
                "ascending"
            }
        ));
    }

    fn on_logs_key(&mut self, key: KeyEvent) {
        let page = 10;
        match key.code {
            KeyCode::Up => self.logs.scroll = self.logs.scroll.saturating_add(1),
            KeyCode::Down => self.logs.scroll = self.logs.scroll.saturating_sub(1),
            KeyCode::PageUp => self.logs.scroll = self.logs.scroll.saturating_add(page),
            KeyCode::PageDown => self.logs.scroll = self.logs.scroll.saturating_sub(page),
            KeyCode::End => self.logs.scroll = 0,
            KeyCode::Home => self.logs.scroll = self.logs.lines.len(),
            _ => {}
        }
    }

    fn pane(&self, focus: Focus) -> &Pane {
        match focus {
            Focus::Local => &self.local,
            Focus::Remote => &self.remote,
        }
    }

    fn pane_mut(&mut self, focus: Focus) -> &mut Pane {
        match focus {
            Focus::Local => &mut self.local,
            Focus::Remote => &mut self.remote,
        }
    }

    fn path_of(&self, pane: PaneKind, name: &str) -> String {
        match pane {
            PaneKind::Local => local_join(&self.local.cwd, name),
            PaneKind::Remote => remote_join(&self.remote.cwd, name),
        }
    }

    async fn init_sftp(&mut self) {
        if !self.connected {
            self.set_status("Not connected");
            return;
        }
        match &self.sftp {
            SftpState::Initializing => {}
            SftpState::Ready => {
                let path = self.remote.cwd.clone();
                self.send(Cmd::SftpList { path }).await;
            }
            _ => {
                self.sftp = SftpState::Initializing;
                self.set_status("Initializing SFTP subsystem…");
                self.log(LogLevel::Info, "opening SFTP subsystem on demand");
                self.send(Cmd::SftpInit).await;
            }
        }
    }

    async fn open_selected(&mut self) {
        let Some(entry) = self.pane(self.focus).selected().cloned() else {
            return;
        };
        if !entry.is_dir {
            self.set_status(format!("{} is not a directory", entry.name));
            return;
        }
        match self.focus {
            Focus::Local => {
                let path = local_join(&self.local.cwd, &entry.name);
                self.send(Cmd::LocalList { path }).await;
            }
            Focus::Remote => {
                if !self.sftp.is_ready() {
                    self.set_status("Press [F3] to initialize the SFTP session first");
                    return;
                }
                let path = remote_join(&self.remote.cwd, &entry.name);
                self.send(Cmd::SftpList { path }).await;
            }
        }
    }

    async fn go_up(&mut self) {
        match self.focus {
            Focus::Local => {
                if let Some(path) = local_parent(&self.local.cwd) {
                    self.send(Cmd::LocalList { path }).await;
                }
            }
            Focus::Remote => {
                if !self.sftp.is_ready() {
                    return;
                }
                let path = remote_parent(&self.remote.cwd);
                self.send(Cmd::SftpList { path }).await;
            }
        }
    }

    async fn refresh_focused(&mut self) {
        match self.focus {
            Focus::Local => {
                let path = self.local.cwd.clone();
                self.send(Cmd::LocalList { path }).await;
            }
            Focus::Remote => {
                if self.sftp.is_ready() {
                    let path = self.remote.cwd.clone();
                    self.send(Cmd::SftpList { path }).await;
                }
            }
        }
    }

    async fn copy_selected(&mut self) {
        let Some(entry) = self.pane(self.focus).selected().cloned() else {
            self.set_status("Nothing selected");
            return;
        };
        let src = match self.focus {
            Focus::Local => {
                Endpoint::Local(PathBuf::from(local_join(&self.local.cwd, &entry.name)))
            }
            Focus::Remote => Endpoint::Remote(remote_join(&self.remote.cwd, &entry.name)),
        };
        let dst_dir = match self.focus {
            Focus::Local => Endpoint::Remote(self.remote.cwd.clone()),
            Focus::Remote => Endpoint::Local(PathBuf::from(&self.local.cwd)),
        };
        let needs_sftp =
            matches!(src, Endpoint::Remote(_)) || matches!(dst_dir, Endpoint::Remote(_));
        if needs_sftp && !self.sftp.is_ready() {
            self.set_status("Press [F3] to initialize the SFTP session first");
            return;
        }

        let id = self.next_transfer_id;
        self.next_transfer_id += 1;
        self.log(
            LogLevel::Info,
            format!("copying {} to the other pane", entry.display_name()),
        );
        self.set_status(format!("Copying {}…", entry.display_name()));
        self.send(Cmd::Transfer { id, src, dst_dir }).await;
    }

    fn open_rename_modal(&mut self) {
        let Some(entry) = self.pane(self.focus).selected().cloned() else {
            self.set_status("Nothing selected");
            return;
        };
        let path = self.path_of(self.focus.kind(), &entry.name);
        self.modal = Some(Modal::Input(InputModal {
            title: "Rename".to_string(),
            label: format!("New name for \"{}\":", entry.name),
            field: TextField::new(entry.name.clone()),
            error: None,
            action: InputAction::Rename {
                pane: self.focus.kind(),
                from: path,
            },
        }));
    }

    fn open_mkdir_modal(&mut self) {
        let parent = self.pane(self.focus).cwd.clone();
        self.modal = Some(Modal::Input(InputModal {
            title: "New directory".to_string(),
            label: format!("Create directory in \"{parent}\":"),
            field: TextField::default(),
            error: None,
            action: InputAction::Mkdir {
                pane: self.focus.kind(),
                parent,
            },
        }));
    }

    /// Resolve a typed directory against the pane's cwd. Absolute input wins;
    /// otherwise the two are joined and folded, so `..` and `sub/./x` work.
    fn resolve_dir(pane: PaneKind, base: &str, value: &str) -> String {
        match pane {
            PaneKind::Local => local_normalize(&local_join(base, value)),
            PaneKind::Remote => remote_normalize(&remote_join(base, value)),
        }
    }

    fn open_chdir_modal(&mut self) {
        if self.focus == Focus::Remote && !self.sftp.is_ready() {
            self.set_status("Press [F3] to initialize the SFTP session first");
            return;
        }
        let pane = self.focus.kind();
        let base = self.pane(self.focus).cwd.clone();
        self.modal = Some(Modal::Input(InputModal {
            title: "Change directory".to_string(),
            label: format!("Directory for {} (relative to {base}):", pane.label()),
            field: TextField::new(base.clone()),
            error: None,
            action: InputAction::ChangeDir { pane, base },
        }));
    }

    fn open_delete_modal(&mut self) {
        let Some(entry) = self.pane(self.focus).selected().cloned() else {
            self.set_status("Nothing selected");
            return;
        };
        let path = self.path_of(self.focus.kind(), &entry.name);
        let what = if entry.is_dir {
            "directory (recursively)"
        } else {
            "file"
        };
        self.modal = Some(Modal::Confirm(ConfirmModal {
            title: "Delete".to_string(),
            message: format!("Delete {what} \"{}\"? This cannot be undone.", entry.name),
            action: ConfirmAction::Delete {
                pane: self.focus.kind(),
                path,
                is_dir: entry.is_dir,
            },
        }));
    }

    async fn on_modal_key(&mut self, key: KeyEvent) {
        let Some(modal) = self.modal.take() else {
            return;
        };
        match modal {
            Modal::HostKey(mut modal) => match key.code {
                KeyCode::Char('y') | KeyCode::Char('Y') | KeyCode::Enter => {
                    if let Some(answer) = modal.answer.take() {
                        let _ = answer.send(true);
                    }
                    self.log(
                        LogLevel::Info,
                        format!("host key for {}:{} accepted", modal.host, modal.port),
                    );
                    self.set_status("Host key accepted");
                }
                KeyCode::Char('n') | KeyCode::Char('N') | KeyCode::Esc => {
                    if let Some(answer) = modal.answer.take() {
                        let _ = answer.send(false);
                    }
                    self.log(LogLevel::Warn, "host key rejected by user");
                    self.set_status("Host key rejected");
                }
                _ => self.modal = Some(Modal::HostKey(modal)),
            },
            Modal::Input(mut modal) => {
                match key.code {
                    // Only a valid Enter and Esc close the dialog. Every other
                    // key edits the field and the modal is put back below, or
                    // the dialog would vanish on the first keystroke.
                    KeyCode::Esc => return,
                    KeyCode::Enter => {
                        let value = modal.field.value.trim().to_string();
                        let is_chdir = matches!(modal.action, InputAction::ChangeDir { .. });
                        if value.is_empty() {
                            modal.error = Some(if is_chdir {
                                "Path cannot be empty".to_string()
                            } else {
                                "Name cannot be empty".to_string()
                            });
                        } else if let InputAction::ChangeDir { pane, base } = &modal.action {
                            let target = Self::resolve_dir(*pane, base, &value);
                            let cmd = match pane {
                                PaneKind::Local => Cmd::LocalList { path: target },
                                PaneKind::Remote => Cmd::SftpList { path: target },
                            };
                            self.send(cmd).await;
                            return;
                        } else if value.contains('/')
                            || value.contains('\\')
                            || value == "."
                            || value == ".."
                        {
                            modal.error = Some(
                                "Name cannot contain path separators or be . or ..".to_string(),
                            );
                        } else {
                            match modal.action {
                                InputAction::Rename { pane, from } => {
                                    self.send(Cmd::Rename {
                                        pane,
                                        from,
                                        to: value,
                                    })
                                    .await;
                                }
                                InputAction::Mkdir { pane, parent } => {
                                    self.send(Cmd::Mkdir {
                                        pane,
                                        parent,
                                        name: value,
                                    })
                                    .await;
                                }
                                InputAction::ChangeDir { .. } => unreachable!(),
                            }
                            return;
                        }
                    }
                    KeyCode::Char('u') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                        modal.field = TextField::default();
                        modal.error = None;
                    }
                    KeyCode::Backspace => modal.field.backspace(),
                    KeyCode::Delete => modal.field.delete(),
                    KeyCode::Left => modal.field.left(),
                    KeyCode::Right => modal.field.right(),
                    KeyCode::Home => modal.field.home(),
                    KeyCode::End => modal.field.end(),
                    KeyCode::Char(c)
                        if !key.modifiers.contains(KeyModifiers::CONTROL)
                            && !key.modifiers.contains(KeyModifiers::ALT) =>
                    {
                        modal.field.insert(c);
                        modal.error = None;
                    }
                    _ => {}
                }
                self.modal = Some(Modal::Input(modal));
            }
            Modal::Confirm(modal) => match key.code {
                KeyCode::Char('y') | KeyCode::Char('Y') | KeyCode::Enter => match modal.action {
                    ConfirmAction::Delete { pane, path, is_dir } => {
                        self.set_status("Deleting…");
                        self.send(Cmd::Delete { pane, path, is_dir }).await;
                    }
                },
                KeyCode::Char('n') | KeyCode::Char('N') | KeyCode::Esc => {}
                _ => self.modal = Some(Modal::Confirm(modal)),
            },
        }
    }

    pub async fn on_event(&mut self, evt: Evt) {
        match evt {
            Evt::Connected { host, port, user } => {
                self.connected = true;
                self.form.connecting = false;
                self.form.error = None;
                self.screen = Screen::Main;
                self.tab = Tab::Terminal;
                self.host_label = format!("{user}@{host}:{port}");
                // The SFTP subsystem belongs to the SSH session: a new session
                // has none, and a `SftpReady` racing the reconnect must not make
                // the old listing look live again.
                self.sftp = SftpState::NotInitialized;
                self.remote = Pane::new("/");
                self.transfer = None;
                self.set_status(format!("Connected to {host}:{port}"));
                self.log(
                    LogLevel::Info,
                    format!("connected to {user}@{host}:{port} — PTY xterm-256color"),
                );
                let path = self.local.cwd.clone();
                self.send(Cmd::LocalList { path }).await;
            }
            Evt::ConnectFailed(message) => {
                self.form.connecting = false;
                self.form.error = Some(message.clone());
                self.connected = false;
                self.log(LogLevel::Error, format!("connection failed: {message}"));
                self.set_status("Connection failed");
            }
            Evt::HostKeyPrompt {
                host,
                port,
                key_type,
                fingerprint,
                answer,
            } => {
                self.log(
                    LogLevel::Warn,
                    format!("unknown host key for {host}:{port} ({key_type})"),
                );
                self.modal = Some(Modal::HostKey(HostKeyModal {
                    host,
                    port,
                    key_type,
                    fingerprint,
                    answer: Some(answer),
                }));
            }
            Evt::HostKeyChanged { host, detail } => {
                self.log(
                    LogLevel::Error,
                    format!("HOST KEY CHANGED for {host}: {detail}"),
                );
                self.log(
                    LogLevel::Error,
                    "refusing to connect; remove the stale entry from known_hosts to continue",
                );
                self.set_status("Host key changed — connection refused");
            }
            Evt::PtyData(bytes) => {
                self.blink_on = true;
                self.parser.process(&bytes);
            }
            Evt::PtyClosed(code) => {
                let reason = match code {
                    Some(code) => format!("Remote shell exited with status {code}"),
                    None => "Remote shell closed".to_string(),
                };
                self.log(LogLevel::Warn, reason.as_str());
                self.set_status(reason.as_str());
                // Only a shell we are actually attached to may send us back:
                // a late `PtyClosed` from a connection that already ended must
                // not eject the session that replaced it.
                if self.connected {
                    self.back_to_login(&reason);
                }
            }
            Evt::LocalListing { path, entries } => {
                self.local.set_entries(path, entries);
            }
            Evt::SftpListing { path, entries } => {
                self.remote.set_entries(path, entries);
            }
            Evt::SftpReady { cwd } => {
                self.sftp = SftpState::Ready;
                self.remote.cwd = cwd.clone();
                self.set_status(format!("SFTP ready at {cwd}"));
                self.log(LogLevel::Info, format!("SFTP session initialized at {cwd}"));
            }
            Evt::SftpFailed(message) => {
                self.sftp = SftpState::Failed(message.clone());
                self.log(
                    LogLevel::Error,
                    format!("SFTP initialization failed: {message}"),
                );
                self.set_status("SFTP initialization failed");
            }
            Evt::Status(status) => self.set_status(status),
            Evt::Error(message) => {
                self.log(LogLevel::Error, message.clone());
                self.set_status(message);
            }
            Evt::Log(level, message) => self.log(level, message),
            Evt::TransferStarted { id, files, bytes } => {
                self.transfer = Some(TransferStatus {
                    id,
                    files_done: 0,
                    files_total: files,
                    bytes_done: 0,
                    bytes_total: bytes,
                });
            }
            Evt::TransferProgress {
                id,
                files_done,
                files_total,
                bytes_done,
                bytes_total,
            } => {
                let active = self.transfer.as_ref().map(|t| t.id) == Some(id);
                if active {
                    self.transfer = Some(TransferStatus {
                        id,
                        files_done,
                        files_total,
                        bytes_done,
                        bytes_total,
                    });
                }
            }
            Evt::TransferDone { id, ok, message } => {
                if self.transfer.as_ref().map(|t| t.id) == Some(id) {
                    self.transfer = None;
                }
                if ok {
                    self.log(LogLevel::Info, format!("transfer complete: {message}"));
                    self.set_status(format!("Transfer complete: {message}"));
                } else {
                    self.log(LogLevel::Error, format!("transfer failed: {message}"));
                    self.set_status(format!("Transfer failed: {message}"));
                }
                let local = self.local.cwd.clone();
                self.send(Cmd::LocalList { path: local }).await;
                if self.sftp.is_ready() {
                    let remote = self.remote.cwd.clone();
                    self.send(Cmd::SftpList { path: remote }).await;
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The receiver is returned so the tests can see what reaches the network
    /// task.
    fn app() -> (App, mpsc::Receiver<Cmd>) {
        let (tx, rx) = mpsc::channel(16);
        (App::new(tx, None), rx)
    }

    fn press(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    fn press_shift(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::SHIFT)
    }

    fn press_ctrl(c: char) -> KeyEvent {
        KeyEvent::new(KeyCode::Char(c), KeyModifiers::CONTROL)
    }

    fn type_text(app: &mut App, text: &str) {
        for c in text.chars() {
            // `on_modal_key` is synchronous in practice, but the public entry
            // point is async so the tests go through it.
            futures::executor::block_on(app.on_modal_key(press(KeyCode::Char(c))));
        }
    }

    fn drain(rx: &mut mpsc::Receiver<Cmd>) {
        while rx.try_recv().is_ok() {}
    }

    fn scrollback(app: &App) -> usize {
        app.parser.screen().scrollback()
    }

    fn input_field(app: &App) -> &TextField {
        match app.modal.as_ref().expect("a modal is open") {
            Modal::Input(modal) => &modal.field,
            _ => panic!("expected the input modal"),
        }
    }

    #[test]
    fn typing_in_the_mkdir_dialog_keeps_it_open() {
        let (mut app, mut rx) = app();
        app.open_mkdir_modal();
        drain(&mut rx);

        type_text(&mut app, "docs");

        assert_eq!(input_field(&app).value, "docs");
        assert_eq!(input_field(&app).cursor, 4);
        assert!(
            matches!(app.modal, Some(Modal::Input(_))),
            "the dialog must survive a keystroke"
        );
        assert!(rx.try_recv().is_err(), "nothing is sent until Enter");
    }

    #[test]
    fn editing_keys_keep_the_mkdir_dialog_open() {
        let (mut app, mut rx) = app();
        app.open_mkdir_modal();
        type_text(&mut app, "ab");
        drain(&mut rx);

        for key in [
            press(KeyCode::Left),
            press(KeyCode::Home),
            press(KeyCode::Right),
            press(KeyCode::End),
            press(KeyCode::Backspace),
            press(KeyCode::Tab),
            press(KeyCode::PageUp),
        ] {
            futures::executor::block_on(app.on_modal_key(key));
            assert!(
                matches!(app.modal, Some(Modal::Input(_))),
                "{key:?} must not close the dialog"
            );
        }
        assert_eq!(input_field(&app).value, "a");
        assert!(rx.try_recv().is_err());
    }

    #[test]
    fn valid_mkdir_name_is_sent_and_closes_the_dialog() {
        let (mut app, mut rx) = app();
        app.open_mkdir_modal();
        let parent = match app.modal.as_ref().unwrap() {
            Modal::Input(modal) => match &modal.action {
                InputAction::Mkdir { parent, .. } => parent.clone(),
                _ => unreachable!(),
            },
            _ => unreachable!(),
        };
        type_text(&mut app, "docs");
        drain(&mut rx);

        futures::executor::block_on(app.on_modal_key(press(KeyCode::Enter)));

        assert!(app.modal.is_none(), "a valid Enter closes the dialog");
        match rx.try_recv() {
            Ok(Cmd::Mkdir {
                name, parent: sent, ..
            }) => {
                assert_eq!(name, "docs");
                assert_eq!(sent, parent);
            }
            Ok(_) => panic!("expected Cmd::Mkdir"),
            Err(err) => panic!("nothing was sent: {err}"),
        }
    }

    #[test]
    fn invalid_mkdir_names_stay_open_with_an_error() {
        for name in ["", "a/b", "a\\b", ".", ".."] {
            let (mut app, mut rx) = app();
            app.open_mkdir_modal();
            type_text(&mut app, name);
            drain(&mut rx);

            futures::executor::block_on(app.on_modal_key(press(KeyCode::Enter)));

            match app.modal.as_ref() {
                Some(Modal::Input(modal)) => {
                    assert!(modal.error.is_some(), "{name:?} needs an error");
                }
                _ => panic!("{name:?} must keep the dialog open"),
            }
            assert!(rx.try_recv().is_err(), "{name:?} must not be sent");
        }
    }

    #[test]
    fn escape_closes_the_dialog_without_sending() {
        let (mut app, mut rx) = app();
        app.open_mkdir_modal();
        type_text(&mut app, "docs");
        drain(&mut rx);

        futures::executor::block_on(app.on_modal_key(press(KeyCode::Esc)));

        assert!(app.modal.is_none());
        assert!(rx.try_recv().is_err());
    }

    #[test]
    fn rename_dialog_is_prefilled_and_editable() {
        let (mut app, mut rx) = app();
        app.remote.set_entries(
            "/home/tester".to_string(),
            vec![FsEntry {
                name: "notes.txt".to_string(),
                is_dir: false,
                is_symlink: false,
                size: 1,
                modified: None,
            }],
        );
        app.screen = Screen::Main;
        app.tab = Tab::Sftp;
        app.focus = Focus::Remote;
        app.open_rename_modal();
        assert_eq!(input_field(&app).value, "notes.txt");
        assert_eq!(input_field(&app).cursor, 9);

        futures::executor::block_on(app.on_modal_key(press(KeyCode::End)));
        type_text(&mut app, ".bak");
        assert_eq!(input_field(&app).value, "notes.txt.bak");
        drain(&mut rx);

        futures::executor::block_on(app.on_modal_key(press(KeyCode::Enter)));

        assert!(app.modal.is_none());
        match rx.try_recv() {
            Ok(Cmd::Rename { to, .. }) => assert_eq!(to, "notes.txt.bak"),
            Ok(_) => panic!("expected Cmd::Rename"),
            Err(err) => panic!("nothing was sent: {err}"),
        }
    }

    /// An app sitting on the SFTP tab with a live subsystem and a remote cwd.
    fn sftp_app(focus: Focus) -> (App, mpsc::Receiver<Cmd>) {
        let (mut app, rx) = app();
        app.screen = Screen::Main;
        app.connected = true;
        app.tab = Tab::Sftp;
        app.focus = focus;
        app.sftp = SftpState::Ready;
        app.remote = Pane::new("/home/tester");
        (app, rx)
    }

    fn chdir_action(app: &App) -> (PaneKind, String) {
        match app.modal.as_ref().expect("a modal is open") {
            Modal::Input(modal) => match &modal.action {
                InputAction::ChangeDir { pane, base } => (*pane, base.clone()),
                _ => panic!("expected the chdir action"),
            },
            _ => panic!("expected the input modal"),
        }
    }

    #[test]
    fn f9_opens_a_prefilled_chdir_dialog() {
        let (mut app, mut rx) = sftp_app(Focus::Remote);
        drain(&mut rx);

        futures::executor::block_on(app.on_key(press(KeyCode::F(9))));

        assert_eq!(input_field(&app).value, "/home/tester");
        assert_eq!(input_field(&app).cursor, 12, "the caret ends up at the end");
        assert_eq!(
            chdir_action(&app),
            (PaneKind::Remote, "/home/tester".into())
        );
        assert!(rx.try_recv().is_err(), "opening a dialog sends nothing");
    }

    #[test]
    fn chdir_remote_absolute_path_is_sent_as_typed() {
        let (mut app, mut rx) = sftp_app(Focus::Remote);
        futures::executor::block_on(app.on_key(press(KeyCode::F(9))));
        drain(&mut rx);

        futures::executor::block_on(app.on_modal_key(press_ctrl('u')));
        type_text(&mut app, "/var/log");
        futures::executor::block_on(app.on_modal_key(press(KeyCode::Enter)));

        assert!(app.modal.is_none());
        match rx.try_recv() {
            Ok(Cmd::SftpList { path }) => assert_eq!(path, "/var/log"),
            Ok(_) => panic!("expected Cmd::SftpList"),
            Err(err) => panic!("nothing was sent: {err}"),
        }
    }

    #[test]
    fn chdir_remote_relative_path_resolves_against_the_pane_cwd() {
        let (mut app, mut rx) = sftp_app(Focus::Remote);
        futures::executor::block_on(app.on_key(press(KeyCode::F(9))));
        drain(&mut rx);

        futures::executor::block_on(app.on_modal_key(press_ctrl('u')));
        type_text(&mut app, "logs");
        futures::executor::block_on(app.on_modal_key(press(KeyCode::Enter)));

        match rx.try_recv() {
            Ok(Cmd::SftpList { path }) => assert_eq!(path, "/home/tester/logs"),
            Ok(_) => panic!("expected Cmd::SftpList"),
            Err(err) => panic!("nothing was sent: {err}"),
        }
    }

    #[test]
    fn chdir_folds_dot_and_dotdot_segments() {
        let (mut app, mut rx) = sftp_app(Focus::Remote);
        futures::executor::block_on(app.on_key(press(KeyCode::F(9))));
        drain(&mut rx);

        futures::executor::block_on(app.on_modal_key(press_ctrl('u')));
        type_text(&mut app, "../shared/./docs");
        futures::executor::block_on(app.on_modal_key(press(KeyCode::Enter)));

        match rx.try_recv() {
            Ok(Cmd::SftpList { path }) => assert_eq!(path, "/home/shared/docs"),
            Ok(_) => panic!("expected Cmd::SftpList"),
            Err(err) => panic!("nothing was sent: {err}"),
        }
    }

    #[test]
    fn chdir_relative_path_never_escapes_the_remote_root() {
        let (mut app, mut rx) = sftp_app(Focus::Remote);
        futures::executor::block_on(app.on_key(press(KeyCode::F(9))));
        drain(&mut rx);

        futures::executor::block_on(app.on_modal_key(press_ctrl('u')));
        type_text(&mut app, "../../../../etc");
        futures::executor::block_on(app.on_modal_key(press(KeyCode::Enter)));

        match rx.try_recv() {
            Ok(Cmd::SftpList { path }) => assert_eq!(path, "/etc"),
            Ok(_) => panic!("expected Cmd::SftpList"),
            Err(err) => panic!("nothing was sent: {err}"),
        }
    }

    #[test]
    fn chdir_on_the_local_pane_lists_locally() {
        let (mut app, mut rx) = sftp_app(Focus::Local);
        futures::executor::block_on(app.on_key(press(KeyCode::F(9))));
        drain(&mut rx);

        futures::executor::block_on(app.on_modal_key(press_ctrl('u')));
        type_text(&mut app, "sub");
        futures::executor::block_on(app.on_modal_key(press(KeyCode::Enter)));

        match rx.try_recv() {
            Ok(Cmd::LocalList { path }) => {
                assert!(path.ends_with("sub"), "{path}");
                assert!(!path.starts_with('/') || cfg!(unix), "{path}");
            }
            Ok(_) => panic!("expected Cmd::LocalList"),
            Err(err) => panic!("nothing was sent: {err}"),
        }
    }

    #[test]
    fn chdir_is_refused_until_sftp_is_ready() {
        let (mut app, mut rx) = sftp_app(Focus::Remote);
        app.sftp = SftpState::NotInitialized;
        drain(&mut rx);

        futures::executor::block_on(app.on_key(press(KeyCode::F(9))));

        assert!(app.modal.is_none(), "no dialog without a subsystem");
        assert!(app.status.contains("[F3]"), "{}", app.status);
        assert!(rx.try_recv().is_err());
    }

    #[test]
    fn find_is_refused_until_sftp_is_ready() {
        let (mut app, mut rx) = sftp_app(Focus::Remote);
        app.sftp = SftpState::NotInitialized;
        drain(&mut rx);

        futures::executor::block_on(app.on_key(press(KeyCode::Char('/'))));

        assert!(!app.remote.is_searching());
        assert!(app.status.contains("[F3]"), "{}", app.status);
        assert!(rx.try_recv().is_err());
    }

    #[test]
    fn empty_chdir_stays_open_with_an_error() {
        let (mut app, mut rx) = sftp_app(Focus::Remote);
        futures::executor::block_on(app.on_key(press(KeyCode::F(9))));
        futures::executor::block_on(app.on_modal_key(press_ctrl('u')));
        drain(&mut rx);

        futures::executor::block_on(app.on_modal_key(press(KeyCode::Enter)));

        match app.modal.as_ref() {
            Some(Modal::Input(modal)) => assert!(modal.error.is_some()),
            _ => panic!("the dialog must stay open"),
        }
        assert!(rx.try_recv().is_err(), "nothing is sent");
    }

    #[test]
    fn escape_closes_the_chdir_dialog_without_sending() {
        let (mut app, mut rx) = sftp_app(Focus::Remote);
        futures::executor::block_on(app.on_key(press(KeyCode::F(9))));
        drain(&mut rx);

        futures::executor::block_on(app.on_modal_key(press(KeyCode::Esc)));

        assert!(app.modal.is_none());
        assert!(rx.try_recv().is_err());
    }

    #[test]
    fn ctrl_u_clears_the_dialog_field() {
        let (mut app, mut rx) = sftp_app(Focus::Remote);
        futures::executor::block_on(app.on_key(press(KeyCode::F(9))));
        drain(&mut rx);

        futures::executor::block_on(app.on_modal_key(press_ctrl('u')));

        assert!(input_field(&app).value.is_empty());
        assert_eq!(input_field(&app).cursor, 0);
        assert!(matches!(app.modal, Some(Modal::Input(_))));
        assert!(rx.try_recv().is_err());
    }

    fn entry(name: &str, is_dir: bool, size: u64) -> FsEntry {
        FsEntry {
            name: name.to_string(),
            is_dir,
            is_symlink: false,
            size,
            modified: None,
        }
    }

    fn visible_names(pane: &Pane) -> Vec<String> {
        pane.visible().map(|entry| entry.name.clone()).collect()
    }

    #[test]
    fn sorting_by_size_keeps_directories_first() {
        let (mut app, mut rx) = sftp_app(Focus::Remote);
        app.remote.set_entries(
            "/home/tester".to_string(),
            vec![
                entry("big.bin", false, 900),
                entry("small.txt", false, 10),
                entry("docs", true, 0),
                entry("archive", true, 0),
            ],
        );
        drain(&mut rx);

        assert_eq!(
            visible_names(&app.remote),
            ["archive", "docs", "big.bin", "small.txt"],
            "the default is name order with directories first"
        );

        futures::executor::block_on(app.on_key(press(KeyCode::Char('s'))));
        assert_eq!(
            visible_names(&app.remote),
            ["archive", "docs", "small.txt", "big.bin"],
            "size order still groups directories first"
        );

        futures::executor::block_on(app.on_key(press(KeyCode::Char('S'))));
        assert_eq!(
            visible_names(&app.remote),
            ["archive", "docs", "big.bin", "small.txt"],
            "reversing sorts the files but keeps the directory group first"
        );
        assert!(rx.try_recv().is_err(), "sorting never leaves the UI");
    }

    #[test]
    fn sorting_keeps_the_selected_name() {
        let (mut app, _rx) = sftp_app(Focus::Remote);
        app.remote.set_entries(
            "/home/tester".to_string(),
            vec![
                entry("archive", true, 0),
                entry("docs", true, 0),
                entry("big.bin", false, 900),
                entry("small.txt", false, 10),
            ],
        );
        app.remote.state.select(Some(2));
        assert_eq!(app.remote.selected_name().as_deref(), Some("big.bin"));

        futures::executor::block_on(app.on_key(press(KeyCode::Char('s'))));

        assert_eq!(app.remote.selected_name().as_deref(), Some("big.bin"));
    }

    #[test]
    fn find_filters_the_listing_and_escape_restores_it() {
        let (mut app, mut rx) = sftp_app(Focus::Remote);
        app.remote.set_entries(
            "/home/tester".to_string(),
            vec![
                entry("docs", true, 0),
                entry("notes.txt", false, 12),
                entry("other.log", false, 34),
            ],
        );
        drain(&mut rx);

        futures::executor::block_on(app.on_key(press(KeyCode::Char('/'))));
        assert!(app.remote.is_searching());
        for c in "NOT".chars() {
            futures::executor::block_on(app.on_key(press(KeyCode::Char(c))));
        }

        assert_eq!(app.remote.filter(), "NOT");
        assert_eq!(visible_names(&app.remote), ["notes.txt"]);
        assert_eq!(app.remote.sort_key(), SortKey::Name, "typing does not sort");
        assert!(rx.try_recv().is_err(), "filtering never leaves the UI");

        futures::executor::block_on(app.on_key(press(KeyCode::Esc)));

        assert!(!app.remote.is_searching());
        assert!(app.remote.filter().is_empty());
        assert_eq!(visible_names(&app.remote).len(), 3);
    }

    #[test]
    fn a_new_directory_drops_the_previous_filter() {
        let (mut app, mut rx) = sftp_app(Focus::Remote);
        app.remote.set_entries(
            "/home/tester".to_string(),
            vec![entry("docs", true, 0), entry("notes.txt", false, 12)],
        );
        futures::executor::block_on(app.on_key(press(KeyCode::Char('/'))));
        for c in "doc".chars() {
            futures::executor::block_on(app.on_key(press(KeyCode::Char(c))));
        }
        drain(&mut rx);

        futures::executor::block_on(app.on_key(press(KeyCode::Enter)));
        match rx.try_recv() {
            Ok(Cmd::SftpList { path }) => assert_eq!(path, "/home/tester/docs"),
            Ok(_) => panic!("expected Cmd::SftpList"),
            Err(err) => panic!("nothing was sent: {err}"),
        }

        futures::executor::block_on(app.on_event(Evt::SftpListing {
            path: "/home/tester/docs".to_string(),
            entries: vec![entry("readme.md", false, 5)],
        }));

        assert!(app.remote.filter().is_empty());
        assert!(!app.remote.is_searching());
        assert_eq!(visible_names(&app.remote), ["readme.md"]);
    }

    #[test]
    fn refreshing_the_same_directory_keeps_the_filter_and_sort() {
        let (mut app, _rx) = sftp_app(Focus::Remote);
        app.remote.set_entries(
            "/home/tester".to_string(),
            vec![entry("alpha.log", false, 30), entry("beta.txt", false, 10)],
        );
        app.remote.cycle_sort();
        app.remote.begin_search();
        app.remote.search_insert('l');
        app.remote.search_commit();

        app.remote.set_entries(
            "/home/tester".to_string(),
            vec![
                entry("alpha.log", false, 30),
                entry("beta.txt", false, 10),
                entry("later.log", false, 20),
            ],
        );

        assert_eq!(app.remote.sort_key(), SortKey::Size);
        assert_eq!(app.remote.filter(), "l");
        assert_eq!(visible_names(&app.remote), ["later.log", "alpha.log"]);
    }

    #[test]
    fn blink_tick_toggles_the_ssh_caret() {
        let (mut app, _rx) = app();
        assert!(app.blink_on, "the caret starts solid");
        app.blink_tick();
        assert!(!app.blink_on);
        app.blink_tick();
        assert!(app.blink_on);
    }

    #[test]
    fn typing_and_output_reset_the_blink_phase() {
        let (mut app, mut rx) = app();
        app.screen = Screen::Main;
        app.tab = Tab::Terminal;
        drain(&mut rx);

        app.blink_on = false;
        futures::executor::block_on(app.on_key(press(KeyCode::Char('a'))));
        assert!(app.blink_on, "a keystroke makes the caret solid again");

        app.blink_on = false;
        futures::executor::block_on(app.on_event(Evt::PtyData(b"x".to_vec())));
        assert!(app.blink_on, "new output makes the caret solid again");
    }

    #[test]
    fn an_ssh_config_alias_prefills_the_form_when_host_is_left() {
        let (mut app, mut rx) = app();
        drain(&mut rx);
        app.set_ssh_config(SshConfig::parse(
            "Host prod\n    HostName 10.0.0.5\n    User deploy\n    Port 2222\n    IdentityFile ~/.ssh/prod\n",
        ));

        app.form.host = TextField::new("prod");
        app.form.focus = Field::Host;
        futures::executor::block_on(app.on_key(press(KeyCode::Tab)));

        assert_eq!(app.form.host.value, "10.0.0.5");
        assert_eq!(app.form.username.value, "deploy");
        assert_eq!(app.form.port.value, "2222");
        assert_eq!(app.form.key_path.value, "~/.ssh/prod");
        assert_eq!(app.form.focus, Field::Port, "Tab still moves on");

        let fill = app.form.config.as_ref().expect("the source note is set");
        assert_eq!(fill.alias, "prod");
        assert_eq!(fill.hostname, "10.0.0.5");
        assert_eq!(fill.endpoint, "deploy@10.0.0.5:2222");
        assert!(rx.try_recv().is_err(), "a lookup sends no command");
    }

    #[test]
    fn ssh_config_only_fills_values_left_at_their_defaults() {
        let (mut app, mut rx) = app();
        drain(&mut rx);
        app.set_ssh_config(SshConfig::parse(
            "Host prod\n    HostName 10.0.0.5\n    User deploy\n    Port 2222\n",
        ));

        app.form.host = TextField::new("prod");
        app.form.username = TextField::new("someone");
        app.form.port = TextField::new("2200");
        app.form.password = TextField::new("secret");
        futures::executor::block_on(app.on_key(press(KeyCode::Enter)));

        let params = match rx.try_recv().expect("the connection is sent") {
            Cmd::Connect(params) => params,
            _ => panic!("expected a connect command"),
        };
        assert_eq!(params.host, "10.0.0.5", "the alias is always resolved");
        assert_eq!(params.username, "someone", "typed username wins");
        assert_eq!(params.port, 2200, "typed port wins");
    }

    #[test]
    fn a_proxy_alias_is_refused_instead_of_dialled_directly() {
        let (mut app, mut rx) = app();
        drain(&mut rx);
        app.set_ssh_config(SshConfig::parse(
            "Host bastion-only\n    ProxyJump bastion\n",
        ));

        app.form.host = TextField::new("bastion-only");
        app.form.username = TextField::new("deploy");
        app.form.password = TextField::new("secret");
        futures::executor::block_on(app.on_key(press(KeyCode::Enter)));

        let error = app.form.error.as_deref().expect("the form refuses");
        assert!(error.contains("ProxyJump bastion"), "{error}");
        assert!(error.contains("not supported"), "{error}");
        assert!(!app.form.connecting);
        assert!(rx.try_recv().is_err(), "nothing was dialled");
    }

    #[test]
    fn a_command_line_alias_is_resolved_before_the_first_frame() {
        let (tx, _rx) = mpsc::channel(8);
        let mut app = App::new(tx, Some("prod".to_string()));
        app.set_ssh_config(SshConfig::parse(
            "Host prod\n    HostName 10.0.0.5\n    User deploy\n",
        ));

        assert_eq!(app.form.host.value, "10.0.0.5");
        assert_eq!(app.form.username.value, "deploy");
        assert!(app.form.config.is_some());
    }

    #[test]
    fn shift_navigation_scrolls_the_terminal_history_locally() {
        let (mut app, mut rx) = app();
        app.screen = Screen::Main;
        app.tab = Tab::Terminal;
        app.on_resize(100, 30);
        let lines = 200;
        for i in 0..lines {
            app.parser.process(format!("line {i}\r\n").as_bytes());
        }
        drain(&mut rx);

        let page = app.pty_rows as usize - 1;

        // Ask vt100 how much history it actually kept, rather than assuming.
        app.parser.screen_mut().set_scrollback(usize::MAX);
        let retained = scrollback(&app);
        assert!(retained > page * 2, "not enough history to page through");
        app.parser.screen_mut().set_scrollback(0);

        futures::executor::block_on(app.on_terminal_key(press_shift(KeyCode::PageUp)));
        assert_eq!(scrollback(&app), page);
        assert!(rx.try_recv().is_err(), "Shift+PgUp is local only");

        futures::executor::block_on(app.on_terminal_key(press_shift(KeyCode::PageUp)));
        assert_eq!(scrollback(&app), page * 2);

        futures::executor::block_on(app.on_terminal_key(press_shift(KeyCode::PageDown)));
        assert_eq!(scrollback(&app), page);

        futures::executor::block_on(app.on_terminal_key(press_shift(KeyCode::Home)));
        assert_eq!(
            scrollback(&app),
            retained,
            "Shift+Home reaches the oldest line"
        );
        assert!(rx.try_recv().is_err());

        // Already at the top: paging further up must not run off the end.
        futures::executor::block_on(app.on_terminal_key(press_shift(KeyCode::PageUp)));
        assert_eq!(scrollback(&app), retained);

        futures::executor::block_on(app.on_terminal_key(press_shift(KeyCode::End)));
        assert_eq!(scrollback(&app), 0);
        futures::executor::block_on(app.on_terminal_key(press_shift(KeyCode::PageDown)));
        assert_eq!(
            scrollback(&app),
            0,
            "paging down at the bottom does nothing"
        );

        // Unmodified navigation still belongs to the remote.
        futures::executor::block_on(app.on_terminal_key(press(KeyCode::PageUp)));
        assert_eq!(scrollback(&app), 0);
        assert!(matches!(rx.try_recv(), Ok(Cmd::PtyInput(_))));
    }

    #[test]
    fn connecting_clears_the_previous_session_history() {
        let (mut app, mut rx) = app();
        app.parser.process(b"previous session output\r\n");
        for i in 0..60 {
            app.parser.process(format!("line {i}\r\n").as_bytes());
        }
        app.parser.screen_mut().set_scrollback(10);
        assert!(app.parser.screen().scrollback() > 0);

        app.form.host = TextField::new("example.com");
        app.form.username = TextField::new("tester");
        app.form.password = TextField::new("secret");
        futures::executor::block_on(app.submit_connect());
        drain(&mut rx);

        assert_eq!(app.parser.screen().scrollback(), 0);
        // The old output is gone, so the offset cannot be restored either.
        app.parser.screen_mut().set_scrollback(usize::MAX);
        assert_eq!(app.parser.screen().scrollback(), 0);
        assert!(
            app.parser.screen().contents().trim().is_empty(),
            "the previous session's screen must not be shown again"
        );
    }

    /// An app attached to a live session, with everything that session owns
    /// filled in, so a disconnect has something to tear down.
    fn shell_app() -> (App, mpsc::Receiver<Cmd>) {
        let (mut app, rx) = app();
        app.screen = Screen::Main;
        app.connected = true;
        app.host_label = "tester@127.0.0.1:2222".to_string();
        app.form.host = TextField::new("127.0.0.1");
        app.form.username = TextField::new("tester");
        app.form.password = TextField::new("testpass");
        app.sftp = SftpState::Ready;
        app.remote = Pane::new("/home/tester");
        app.transfer = Some(TransferStatus {
            id: 1,
            files_done: 0,
            files_total: 1,
            bytes_done: 0,
            bytes_total: 12,
        });
        app.modal = Some(Modal::Confirm(ConfirmModal {
            title: "Delete".to_string(),
            message: "delete notes.txt?".to_string(),
            action: ConfirmAction::Delete {
                pane: PaneKind::Remote,
                path: "/home/tester/notes.txt".to_string(),
                is_dir: false,
            },
        }));
        (app, rx)
    }

    #[test]
    fn a_closed_shell_returns_to_the_login_screen() {
        let (mut app, _rx) = shell_app();

        futures::executor::block_on(app.on_event(Evt::PtyClosed(Some(0))));

        assert_eq!(app.screen, Screen::Connect, "the login dialog comes back");
        assert!(!app.connected);
        assert!(!app.form.connecting);
        // Blank form: only the default port and the key path are left.
        assert!(app.form.host.value.is_empty());
        assert!(app.form.username.value.is_empty());
        assert!(app.form.password.value.is_empty());
        assert_eq!(app.form.port.value, "22");
        assert_eq!(app.form.focus, Field::Host);
        let notice = app.form.error.as_deref().unwrap_or_default();
        assert!(notice.contains("status 0"), "{notice}");
        assert!(notice.contains("sign in again"), "{notice}");
        // Nothing may still look live.
        assert!(app.modal.is_none());
        assert_eq!(app.sftp, SftpState::NotInitialized);
        assert!(app.remote.entries.is_empty());
        assert!(app.transfer.is_none());
        assert_eq!(app.host_label, "sshftp-tui");
    }

    #[test]
    fn a_shell_close_without_a_session_leaves_the_form_alone() {
        let (mut app, _rx) = app();
        app.form.host = TextField::new("example.com");
        app.form.password = TextField::new("secret");

        futures::executor::block_on(app.on_event(Evt::PtyClosed(None)));

        assert_eq!(app.screen, Screen::Connect);
        assert_eq!(app.form.host.value, "example.com");
        assert_eq!(app.form.password.value, "secret");
        assert!(
            app.form.error.is_none(),
            "a session we never had must not explain itself"
        );
    }

    #[test]
    fn a_new_session_forgets_the_previous_sftp_state() {
        let (mut app, mut rx) = shell_app();

        futures::executor::block_on(app.on_event(Evt::Connected {
            host: "127.0.0.1".to_string(),
            port: 2222,
            user: "tester".to_string(),
        }));
        drain(&mut rx);

        assert_eq!(app.screen, Screen::Main);
        assert!(app.connected);
        // The subsystem lived on the old connection, so F3 is needed again.
        assert_eq!(app.sftp, SftpState::NotInitialized);
        assert!(app.remote.entries.is_empty());
        assert!(app.transfer.is_none());
    }
}
