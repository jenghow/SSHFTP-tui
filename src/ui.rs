//! Rendering. Pure presentation: reads [`App`] and draws widgets.

use ratatui::Frame;
use ratatui::layout::{Alignment, Constraint, Layout, Position, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Cell, Clear, Paragraph, Row, Table, Tabs, Wrap};

use crate::app::{App, Focus, Modal, Pane, Screen, SftpState, Tab};
use crate::event::{LogLevel, PaneKind};
use crate::form::{AuthKind, Field};

pub fn draw(frame: &mut Frame, app: &mut App) {
    match app.screen {
        Screen::Connect => draw_connect(frame, app),
        Screen::Main => draw_main(frame, app),
    }
    draw_modal(frame, app);
}

fn draw_main(frame: &mut Frame, app: &mut App) {
    let chunks = Layout::vertical([
        Constraint::Length(3),
        Constraint::Min(1),
        Constraint::Length(1),
    ])
    .split(frame.area());

    draw_tabs(frame, app, chunks[0]);
    match app.tab {
        Tab::Terminal => draw_terminal(frame, app, chunks[1]),
        Tab::Sftp => draw_sftp(frame, app, chunks[1]),
        Tab::Logs => draw_logs(frame, app, chunks[1]),
    }
    draw_footer(frame, app, chunks[2]);
}

fn draw_tabs(frame: &mut Frame, app: &App, area: Rect) {
    let titles = ["[1] Terminal", "[2] SFTP", "[3] Logs"]
        .iter()
        .map(|title| Line::from(format!(" {title} ")))
        .collect::<Vec<_>>();

    let tabs = Tabs::new(titles)
        .select(app.tab.index())
        .divider("|")
        .style(Style::new().fg(Color::Gray))
        .highlight_style(
            Style::new()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD | Modifier::REVERSED),
        )
        .block(
            Block::bordered()
                .title(format!(" {} ", app.host_label))
                .border_style(Style::new().fg(Color::DarkGray)),
        );

    frame.render_widget(tabs, area);
}

fn draw_terminal(frame: &mut Frame, app: &mut App, area: Rect) {
    let block = Block::bordered();
    let inner = block.inner(area);
    app.fit_pty(inner.width, inner.height);

    let screen = app.parser.screen();
    let scrolled = screen.scrollback();
    let (title, border) = if scrolled > 0 {
        (
            format!(
                " {} — SCROLLBACK +{scrolled}  (Shift+End returns) ",
                app.host_label
            ),
            Color::Yellow,
        )
    } else {
        (format!(" {} ", app.host_label), Color::DarkGray)
    };
    let block = block.title(title).border_style(Style::new().fg(border));

    let (rows, cols) = screen.size();
    let mut lines: Vec<Line> = Vec::with_capacity(rows as usize);

    for row in 0..rows {
        let mut spans: Vec<Span> = Vec::new();
        let mut text = String::new();
        let mut style = Style::default();

        for col in 0..cols {
            let Some(cell) = screen.cell(row, col) else {
                continue;
            };
            if cell.is_wide_continuation() {
                continue;
            }
            let cell_style = cell_style(cell);
            if cell_style != style && !text.is_empty() {
                spans.push(Span::styled(std::mem::take(&mut text), style));
            }
            style = cell_style;
            if cell.has_contents() {
                text.push_str(cell.contents());
            } else {
                text.push(' ');
            }
        }
        if !text.is_empty() {
            spans.push(Span::styled(text, style));
        }
        lines.push(Line::from(spans));
    }

    // `cursor_position` is the live grid's, so showing it while the view is
    // scrolled back would park the caret over unrelated history.
    let cursor = if screen.hide_cursor() || scrolled > 0 {
        None
    } else {
        Some(screen.cursor_position())
    };

    frame.render_widget(Paragraph::new(lines).block(block), area);

    if let Some((row, col)) = cursor {
        if row < inner.height && col < inner.width {
            frame.set_cursor_position(Position::new(inner.x + col, inner.y + row));
        }
    }
}

fn cell_style(cell: &vt100::Cell) -> Style {
    let mut foreground = map_color(cell.fgcolor());
    let mut background = map_color(cell.bgcolor());
    if cell.inverse() {
        std::mem::swap(&mut foreground, &mut background);
    }

    let mut style = Style::new().fg(foreground).bg(background);
    if cell.bold() {
        style = style.add_modifier(Modifier::BOLD);
    }
    if cell.dim() {
        style = style.add_modifier(Modifier::DIM);
    }
    if cell.italic() {
        style = style.add_modifier(Modifier::ITALIC);
    }
    if cell.underline() {
        style = style.add_modifier(Modifier::UNDERLINED);
    }
    style
}

fn map_color(color: vt100::Color) -> Color {
    match color {
        vt100::Color::Default => Color::Reset,
        vt100::Color::Idx(index) => Color::Indexed(index),
        vt100::Color::Rgb(r, g, b) => Color::Rgb(r, g, b),
    }
}

fn draw_sftp(frame: &mut Frame, app: &mut App, area: Rect) {
    let halves =
        Layout::horizontal([Constraint::Percentage(50), Constraint::Percentage(50)]).split(area);
    let focus = app.focus;

    draw_pane(
        frame,
        &mut app.local,
        PaneKind::Local,
        focus == Focus::Local,
        halves[0],
    );

    match app.sftp.clone() {
        SftpState::Ready => draw_pane(
            frame,
            &mut app.remote,
            PaneKind::Remote,
            focus == Focus::Remote,
            halves[1],
        ),
        other => draw_placeholder(frame, halves[1], &other, focus == Focus::Remote),
    }
}

fn draw_pane(frame: &mut Frame, pane: &mut Pane, kind: PaneKind, focused: bool, area: Rect) {
    let border = if focused {
        Color::Yellow
    } else {
        Color::DarkGray
    };
    let title = format!(" {}: {} ", kind.label(), pane.cwd);

    let header = Row::new(vec![
        Cell::from("Name"),
        Cell::from("Size"),
        Cell::from("Date"),
    ])
    .style(Style::new().fg(Color::Gray).add_modifier(Modifier::BOLD));

    let rows = pane.entries.iter().map(|entry| {
        let name_style = if entry.is_dir {
            Style::new().fg(Color::LightCyan)
        } else if entry.is_symlink {
            Style::new().fg(Color::LightMagenta)
        } else {
            Style::new().fg(Color::Gray)
        };
        Row::new(vec![
            Cell::from(entry.display_name()).style(name_style),
            Cell::from(entry.size_text()).style(Style::new().fg(Color::DarkGray)),
            Cell::from(entry.date_text()).style(Style::new().fg(Color::DarkGray)),
        ])
    });

    let table = Table::new(
        rows,
        [
            Constraint::Min(16),
            Constraint::Length(9),
            Constraint::Length(12),
        ],
    )
    .header(header)
    .block(
        Block::bordered()
            .title(title)
            .border_style(Style::new().fg(border)),
    )
    .row_highlight_style(
        Style::new()
            .bg(Color::DarkGray)
            .add_modifier(Modifier::BOLD),
    )
    .highlight_symbol("> ");

    frame.render_stateful_widget(table, area, &mut pane.state);
}

fn draw_placeholder(frame: &mut Frame, area: Rect, state: &SftpState, focused: bool) {
    let border = if focused {
        Color::Yellow
    } else {
        Color::DarkGray
    };

    let lines: Vec<Line> = match state {
        SftpState::NotInitialized => vec![
            Line::from(""),
            Line::from("SFTP session is not active."),
            Line::from(""),
            Line::from("Press [F3] to initialize"),
            Line::from("SFTP connection to the remote host"),
        ],
        SftpState::Initializing => vec![Line::from(""), Line::from("Opening SFTP subsystem…")],
        SftpState::Failed(error) => vec![
            Line::from(""),
            Line::from("SFTP initialization failed:"),
            Line::from(error.clone()),
            Line::from(""),
            Line::from("Press [F3] to retry"),
        ],
        SftpState::Ready => Vec::new(),
    };

    let paragraph = Paragraph::new(lines)
        .alignment(Alignment::Center)
        .wrap(Wrap { trim: true })
        .block(
            Block::bordered()
                .title(" REMOTE ")
                .border_style(Style::new().fg(border)),
        );

    frame.render_widget(paragraph, area);
}

fn draw_logs(frame: &mut Frame, app: &mut App, area: Rect) {
    let block = Block::bordered()
        .title(" Logs ")
        .border_style(Style::new().fg(Color::DarkGray));
    let inner = block.inner(area);

    let visible = inner.height as usize;
    let total = app.logs.lines.len();
    let max_scroll = total.saturating_sub(visible);
    let scroll = app.logs.scroll.min(max_scroll);
    app.logs.scroll = scroll;
    let start = total.saturating_sub(visible + scroll);

    let lines: Vec<Line> = app
        .logs
        .lines
        .iter()
        .skip(start)
        .take(visible)
        .map(|entry| {
            let color = match entry.level {
                LogLevel::Info => Color::Gray,
                LogLevel::Warn => Color::Yellow,
                LogLevel::Error => Color::LightRed,
            };
            Line::from(vec![
                Span::styled(format!("{} ", entry.ts), Style::new().fg(Color::DarkGray)),
                Span::styled(
                    format!("{} ", entry.level.label()),
                    Style::new().fg(color).add_modifier(Modifier::BOLD),
                ),
                Span::styled(entry.text.clone(), Style::new().fg(color)),
            ])
        })
        .collect();

    frame.render_widget(Paragraph::new(lines).block(block), area);
}

fn draw_footer(frame: &mut Frame, app: &App, area: Rect) {
    let hints = match app.tab {
        Tab::Terminal => "[Ctrl+T] Tab Change   [F10] Exit".to_string(),
        Tab::Sftp => format!(
            "Focus: {}   F3 Init   Tab Pane   Enter Open   F4 Refresh   F5 Copy   F6 Rename   F7 Mkdir   F8 Delete",
            app.focus.label()
        ),
        Tab::Logs => {
            "[Up/Down] Scroll   [End] Follow   [Ctrl+T] Tab Change   [F10] Exit".to_string()
        }
    };

    let status = match &app.transfer {
        Some(transfer) => transfer.summary(),
        None => app.status.clone(),
    };
    let status_style = if app.transfer.is_some() {
        Style::new().fg(Color::Yellow).add_modifier(Modifier::BOLD)
    } else {
        Style::new().fg(Color::LightGreen)
    };

    let padding =
        (area.width as usize).saturating_sub(hints.chars().count() + status.chars().count() + 3);

    let line = Line::from(vec![
        Span::styled(format!(" {hints}"), Style::new().fg(Color::DarkGray)),
        Span::raw(" ".repeat(padding)),
        Span::styled(status, status_style),
        Span::raw(" "),
    ]);

    frame.render_widget(Paragraph::new(line), area);
}

fn draw_connect(frame: &mut Frame, app: &mut App) {
    let fields = app.form.visible_fields();
    let mut lines: Vec<Line> = Vec::new();
    let mut cursor: Option<(u16, u16)> = None;

    for field in fields {
        let focused = app.form.focus == field;
        let value_style = if focused {
            Style::new().fg(Color::Yellow)
        } else {
            Style::new().fg(Color::White)
        };
        let label = Span::styled(
            format!("{:<11}", field.label()),
            Style::new().fg(Color::DarkGray),
        );
        // The value is drawn as ` [ {value} ]`, i.e. three columns after the
        // padded label, and the caret sits one column further per character
        // before the insertion point.
        let text_column = (label.content.chars().count() + " [ ".chars().count()) as u16;

        match field {
            Field::Auth => {
                let password_mark = if app.form.auth == AuthKind::Password {
                    "(•)"
                } else {
                    "( )"
                };
                let key_mark = if app.form.auth == AuthKind::KeyFile {
                    "(•)"
                } else {
                    "( )"
                };
                lines.push(Line::from(vec![
                    label,
                    Span::styled(
                        format!(" {password_mark} Password    {key_mark} Key file"),
                        value_style,
                    ),
                ]));
            }
            _ => {
                let Some(state) = app.form.field(field) else {
                    continue;
                };
                let shown = if matches!(field, Field::Password | Field::Passphrase) {
                    "•".repeat(state.char_len())
                } else {
                    state.value.clone()
                };
                lines.push(Line::from(vec![
                    label,
                    Span::styled(format!(" [ {shown} ]"), value_style),
                ]));
                if focused && !app.form.connecting {
                    cursor = Some((lines.len() as u16 - 1, text_column + state.cursor as u16));
                }
            }
        }
    }

    lines.push(Line::from(""));
    if let Some(error) = &app.form.error {
        lines.push(Line::from(Span::styled(
            format!(" {error}"),
            Style::new().fg(Color::LightRed),
        )));
    } else if app.form.connecting {
        lines.push(Line::from(Span::styled(
            " Connecting…",
            Style::new().fg(Color::Yellow),
        )));
    } else {
        lines.push(Line::from(Span::styled(
            " Enter: connect    Tab: next field    Left/Right: switch auth    Esc: quit",
            Style::new().fg(Color::DarkGray),
        )));
    }

    let width = 74.min(frame.area().width.saturating_sub(2)).max(30);
    let height = (lines.len() as u16 + 2).min(frame.area().height);
    let rect = centered(frame.area(), width, height);
    frame.render_widget(Clear, rect);

    let block = Block::bordered()
        .title(" Connect to SSH server ")
        .border_style(Style::new().fg(Color::Cyan));
    let inner = block.inner(rect);
    frame.render_widget(Paragraph::new(lines).block(block), rect);

    if let Some((row, col)) = cursor {
        if row < inner.height && col < inner.width {
            frame.set_cursor_position(Position::new(inner.x + col, inner.y + row));
        }
    }
}

fn draw_modal(frame: &mut Frame, app: &App) {
    let Some(modal) = &app.modal else {
        return;
    };
    let area = frame.area();

    match modal {
        Modal::HostKey(host_key) => {
            let lines = vec![
                Line::from(""),
                Line::from(format!(
                    "The authenticity of host '{}:{}' can't be established.",
                    host_key.host, host_key.port
                )),
                Line::from(""),
                Line::from(format!(
                    "{} key fingerprint is {}.",
                    host_key.key_type, host_key.fingerprint
                )),
                Line::from(""),
                Line::from(Span::styled(
                    "Are you sure you want to continue connecting?  [y/N]",
                    Style::new().fg(Color::Yellow),
                )),
            ];
            render_box(
                frame,
                area,
                78,
                " Host key verification ",
                lines,
                Color::Yellow,
            );
        }
        Modal::Input(input) => {
            let mut lines = vec![
                Line::from(""),
                Line::from(format!(" {}", input.label)),
                Line::from(""),
                Line::from(Span::styled(
                    format!(" [ {} ]", input.field.value),
                    Style::new().fg(Color::White),
                )),
            ];
            if let Some(error) = &input.error {
                lines.push(Line::from(Span::styled(
                    format!(" {error}"),
                    Style::new().fg(Color::LightRed),
                )));
            }
            lines.push(Line::from(""));
            lines.push(Line::from(Span::styled(
                " Enter: confirm    Esc: cancel",
                Style::new().fg(Color::DarkGray),
            )));

            let height = (lines.len() as u16 + 2).min(area.height);
            let rect = centered(area, 64, height);
            frame.render_widget(Clear, rect);
            let block = Block::bordered()
                .title(format!(" {} ", input.title))
                .border_style(Style::new().fg(Color::Cyan));
            let inner = block.inner(rect);
            frame.render_widget(Paragraph::new(lines).block(block), rect);

            let row = 3;
            let col = 3 + input.field.cursor as u16;
            if row < inner.height && col < inner.width {
                frame.set_cursor_position(Position::new(inner.x + col, inner.y + row));
            }
        }
        Modal::Confirm(confirm) => {
            let lines = vec![
                Line::from(""),
                Line::from(format!(" {}", confirm.message)),
                Line::from(""),
                Line::from(Span::styled(
                    "[y] Yes    [n] No",
                    Style::new().fg(Color::Yellow),
                )),
            ];
            render_box(
                frame,
                area,
                72,
                &format!(" {} ", confirm.title),
                lines,
                Color::LightRed,
            );
        }
    }
}

fn render_box(
    frame: &mut Frame,
    area: Rect,
    width: u16,
    title: &str,
    lines: Vec<Line>,
    color: Color,
) {
    let height = (lines.len() as u16 + 2).min(area.height);
    let rect = centered(area, width, height);
    frame.render_widget(Clear, rect);
    let block = Block::bordered()
        .title(title.to_string())
        .border_style(Style::new().fg(color));
    frame.render_widget(
        Paragraph::new(lines)
            .wrap(Wrap { trim: false })
            .block(block),
        rect,
    );
}

fn centered(area: Rect, width: u16, height: u16) -> Rect {
    let width = width.min(area.width);
    let height = height.min(area.height);
    Rect {
        x: area.x + (area.width - width) / 2,
        y: area.y + (area.height - height) / 2,
        width,
        height,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::{App, HostKeyModal, InputAction, InputModal, SftpState};
    use crate::event::{Cmd, FsEntry, LogLevel};
    use crate::form::TextField;
    use ratatui::Terminal;
    use ratatui::backend::{Backend, TestBackend};
    use tokio::sync::mpsc;

    /// The receiver is returned so the channel stays open for the test.
    fn app() -> (App, mpsc::Receiver<Cmd>) {
        let (tx, rx) = mpsc::channel(8);
        (App::new(tx, None), rx)
    }

    fn file(name: &str, size: u64) -> FsEntry {
        FsEntry {
            name: name.to_string(),
            is_dir: false,
            is_symlink: false,
            size,
            modified: None,
        }
    }

    fn dir(name: &str) -> FsEntry {
        FsEntry {
            name: name.to_string(),
            is_dir: true,
            is_symlink: false,
            size: 0,
            modified: None,
        }
    }

    fn screen_text(terminal: &Terminal<TestBackend>) -> String {
        let buffer = terminal.backend().buffer();
        let area = buffer.area;
        let mut text = String::new();
        for y in 0..area.height {
            for x in 0..area.width {
                text.push_str(buffer.content[(y * area.width + x) as usize].symbol());
            }
            text.push('\n');
        }
        text
    }

    fn render(app: &mut App, width: u16, height: u16) -> String {
        let backend = TestBackend::new(width, height);
        let mut terminal = Terminal::new(backend).expect("test terminal");
        terminal
            .draw(|frame| draw(frame, app))
            .expect("rendering never fails on a test backend");

        screen_text(&terminal)
    }

    /// Renders and also reports where the frame asked the terminal to put the
    /// caret.
    fn render_with_cursor(app: &mut App, width: u16, height: u16) -> (String, Position) {
        let backend = TestBackend::new(width, height);
        let mut terminal = Terminal::new(backend).expect("test terminal");
        terminal
            .draw(|frame| draw(frame, app))
            .expect("rendering never fails on a test backend");

        let text = screen_text(&terminal);
        let cursor = terminal
            .backend_mut()
            .get_cursor_position()
            .expect("the test backend never fails");
        (text, cursor)
    }

    /// Row and first *column* of `needle` in rendered screen text. Columns count
    /// characters, matching the coordinates the frame reports to the terminal.
    fn locate(text: &str, needle: &str) -> (u16, u16) {
        let row = text
            .lines()
            .position(|line| line.contains(needle))
            .unwrap_or_else(|| panic!("{needle} is not on screen:\n{text}"));
        let line = text.lines().nth(row).unwrap();
        let byte = line.find(needle).unwrap();
        (row as u16, line[..byte].chars().count() as u16)
    }

    #[test]
    fn connect_screen_shows_every_field() {
        let (mut app, _rx) = app();
        let text = render(&mut app, 100, 30);

        assert!(text.contains("Connect to SSH server"), "{text}");
        for label in ["Host", "Port", "Username", "Auth", "Password"] {
            assert!(text.contains(label), "missing {label}:\n{text}");
        }
    }

    #[test]
    fn connect_screen_reports_why_it_came_back() {
        let (mut app, _rx) = app();
        app.form.error = Some("Remote shell closed — sign in again".to_string());

        let text = render(&mut app, 100, 30);

        assert!(text.contains("Connect to SSH server"), "{text}");
        assert!(text.contains("Remote shell closed"), "{text}");
        assert!(text.contains("sign in again"), "{text}");
    }

    #[test]
    fn connect_form_caret_sits_right_of_the_last_character() {
        let (mut app, _rx) = app();
        app.form.host = TextField::new("example.com");

        let (text, cursor) = render_with_cursor(&mut app, 100, 30);
        let (row, first) = locate(&text, "example.com");

        // The caret is an insertion point, so at the end of the text it lands on
        // the blank cell immediately right of the final character.
        let line = text.lines().nth(row as usize).unwrap();
        assert_eq!(line.chars().nth(first as usize + 10), Some('m'), "{text}");
        assert_eq!(cursor, Position::new(first + 11, row), "{text}");
    }

    #[test]
    fn connect_form_caret_tracks_the_cursor_index() {
        let (mut app, _rx) = app();
        app.form.host = TextField::new("example.com");
        app.form.host.home();

        let (text, cursor) = render_with_cursor(&mut app, 100, 30);
        let (row, first) = locate(&text, "example.com");
        assert_eq!(cursor, Position::new(first, row), "{text}");

        app.form.host.end();
        let (_, cursor) = render_with_cursor(&mut app, 100, 30);
        assert_eq!(cursor, Position::new(first + 11, row), "{text}");
    }

    #[test]
    fn sftp_tab_asks_for_f3_until_initialized() {
        let (mut app, _rx) = app();
        app.screen = Screen::Main;
        app.tab = Tab::Sftp;

        let text = render(&mut app, 120, 30);
        assert!(text.contains("Press [F3] to initialize"), "{text}");
        assert!(text.contains("LOCAL"), "{text}");
        assert!(text.contains("REMOTE"), "{text}");
        assert!(!text.contains("Opening SFTP subsystem"), "{text}");
    }

    #[test]
    fn sftp_pane_renders_entries_with_size_and_directory_marker() {
        let (mut app, _rx) = app();
        app.screen = Screen::Main;
        app.tab = Tab::Sftp;
        app.sftp = SftpState::Ready;
        app.remote.set_entries(
            "/home/tester".to_string(),
            vec![dir("docs"), file("notes.txt", 2048)],
        );

        let text = render(&mut app, 140, 30);
        assert!(text.contains("/home/tester"), "{text}");
        assert!(text.contains("docs/"), "{text}");
        assert!(text.contains("notes.txt"), "{text}");
        assert!(text.contains("2.0K"), "{text}");
    }

    #[test]
    fn terminal_tab_renders_pty_output() {
        let (mut app, _rx) = app();
        app.screen = Screen::Main;
        app.tab = Tab::Terminal;
        app.parser.process(b"\x1b[32mhello\x1b[0m world\r\n$ ");

        let text = render(&mut app, 100, 30);
        assert!(text.contains("hello world"), "{text}");
        assert!(text.contains("$ "), "{text}");
    }

    #[test]
    fn terminal_scrollback_shows_history_and_hides_the_caret() {
        let (mut app, _rx) = app();
        app.screen = Screen::Main;
        app.tab = Tab::Terminal;
        // Pins the PTY to exactly what `draw_terminal` computes, so rendering
        // does not resize the grid and reset the offset being tested.
        app.on_resize(100, 30);
        app.parser.process(b"oldest marker\r\n");
        for i in 0..60 {
            app.parser.process(format!("line {i}\r\n").as_bytes());
        }

        let live = render(&mut app, 100, 30);
        assert!(!live.contains("oldest marker"), "{live}");
        assert!(!live.contains("SCROLLBACK"), "{live}");

        app.parser.screen_mut().set_scrollback(usize::MAX);
        let (text, cursor) = render_with_cursor(&mut app, 100, 30);
        assert!(text.contains("oldest marker"), "{text}");
        assert!(text.contains("SCROLLBACK +"), "{text}");
        assert!(text.contains("Shift+End returns"), "{text}");
        assert_eq!(
            cursor,
            Position::ORIGIN,
            "no caret while scrolled back:\n{text}"
        );

        app.parser.screen_mut().set_scrollback(0);
        let (text, cursor) = render_with_cursor(&mut app, 100, 30);
        assert!(!text.contains("SCROLLBACK"), "{text}");
        assert_ne!(
            cursor,
            Position::ORIGIN,
            "the caret returns with the live view"
        );
    }

    #[test]
    fn logs_tab_shows_level_and_message() {
        let (mut app, _rx) = app();
        app.screen = Screen::Main;
        app.tab = Tab::Logs;
        app.log(LogLevel::Warn, "careful now");

        let text = render(&mut app, 120, 30);
        assert!(text.contains("WARN"), "{text}");
        assert!(text.contains("careful now"), "{text}");
    }

    #[test]
    fn input_modal_caret_sits_right_of_the_last_character() {
        let (mut app, _rx) = app();
        app.modal = Some(Modal::Input(InputModal {
            title: "Rename".to_string(),
            label: "New name:".to_string(),
            field: TextField::new("report.txt"),
            error: None,
            action: InputAction::Mkdir {
                pane: PaneKind::Local,
                parent: "/tmp".to_string(),
            },
        }));

        let (text, cursor) = render_with_cursor(&mut app, 120, 30);
        let (row, first) = locate(&text, "report.txt");

        // Same rule as the connect form: the caret follows the last character.
        assert_eq!(cursor, Position::new(first + 10, row), "{text}");
    }

    #[test]
    fn host_key_modal_shows_fingerprint_and_prompt() {
        let (mut app, _rx) = app();
        app.modal = Some(Modal::HostKey(HostKeyModal {
            host: "example.com".to_string(),
            port: 22,
            key_type: "Ed25519".to_string(),
            fingerprint: "SHA256:abcdef".to_string(),
            answer: None,
        }));

        let text = render(&mut app, 120, 30);
        assert!(text.contains("Host key verification"), "{text}");
        assert!(text.contains("example.com:22"), "{text}");
        assert!(text.contains("SHA256:abcdef"), "{text}");
        assert!(text.contains("continue connecting"), "{text}");
    }

    #[test]
    fn footer_hints_follow_the_active_tab() {
        let (mut app, _rx) = app();
        app.screen = Screen::Main;
        app.tab = Tab::Terminal;
        assert!(render(&mut app, 120, 30).contains("[F10] Exit"));

        app.tab = Tab::Sftp;
        let text = render(&mut app, 120, 30);
        assert!(text.contains("F5 Copy"), "{text}");
        assert!(text.contains("Focus: Left"), "{text}");

        app.focus = Focus::Remote;
        let text = render(&mut app, 120, 30);
        assert!(text.contains("Focus: Right"), "{text}");
    }
}
