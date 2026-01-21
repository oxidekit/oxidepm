//! OxidePM TUI Dashboard
//!
//! Real-time terminal UI for monitoring processes (monit command).

use crossterm::{
    event::{self, DisableMouseCapture, EnableMouseCapture, Event, KeyCode, KeyEventKind},
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use oxidepm_core::{AppInfo, AppStatus};
use oxidepm_ipc::{IpcClient, Request, Response};
use ratatui::{
    backend::CrosstermBackend,
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Cell, Gauge, Paragraph, Row, Table, Tabs},
    Frame, Terminal,
};
use std::io;
use std::path::PathBuf;
use std::time::Duration;

/// TUI Application state
pub struct App {
    client: IpcClient,
    processes: Vec<AppInfo>,
    selected_index: usize,
    tab_index: usize,
    logs: Vec<String>,
    should_quit: bool,
    last_error: Option<String>,
}

impl App {
    pub fn new(socket_path: PathBuf) -> Self {
        Self {
            client: IpcClient::new(socket_path),
            processes: Vec::new(),
            selected_index: 0,
            tab_index: 0,
            logs: Vec::new(),
            should_quit: false,
            last_error: None,
        }
    }

    async fn refresh(&mut self) {
        match self.client.send(&Request::Status).await {
            Ok(Response::Status { apps }) => {
                self.processes = apps;
                self.last_error = None;
                // Adjust selection if needed
                if self.selected_index >= self.processes.len() && !self.processes.is_empty() {
                    self.selected_index = self.processes.len() - 1;
                }
            }
            Ok(Response::Error { message }) => {
                self.last_error = Some(message);
            }
            Err(e) => {
                self.last_error = Some(format!("Connection error: {}", e));
            }
            _ => {}
        }
    }

    async fn refresh_logs(&mut self) {
        if self.processes.is_empty() {
            return;
        }

        let app = &self.processes[self.selected_index];
        let selector = oxidepm_core::Selector::ById(app.spec.id);

        if let Ok(Response::LogLines { lines }) = self.client.send(&Request::Logs {
            selector,
            lines: 50,
            follow: false,
            stdout: true,
            stderr: true,
        }).await {
            self.logs = lines;
        }
    }

    fn next(&mut self) {
        if !self.processes.is_empty() {
            self.selected_index = (self.selected_index + 1) % self.processes.len();
        }
    }

    fn previous(&mut self) {
        if !self.processes.is_empty() {
            self.selected_index = if self.selected_index > 0 {
                self.selected_index - 1
            } else {
                self.processes.len() - 1
            };
        }
    }

    fn next_tab(&mut self) {
        self.tab_index = (self.tab_index + 1) % 3;
    }

    fn previous_tab(&mut self) {
        self.tab_index = if self.tab_index > 0 {
            self.tab_index - 1
        } else {
            2
        };
    }

    async fn stop_selected(&mut self) {
        if self.processes.is_empty() {
            return;
        }

        let app = &self.processes[self.selected_index];
        let selector = oxidepm_core::Selector::ById(app.spec.id);

        let _ = self.client.send(&Request::Stop { selector }).await;
        self.refresh().await;
    }

    async fn restart_selected(&mut self) {
        if self.processes.is_empty() {
            return;
        }

        let app = &self.processes[self.selected_index];
        let selector = oxidepm_core::Selector::ById(app.spec.id);

        let _ = self.client.send(&Request::Restart { selector }).await;
        self.refresh().await;
    }
}

/// Run the TUI application
pub async fn run(socket_path: PathBuf) -> io::Result<()> {
    // Setup terminal
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen, EnableMouseCapture)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    // Create app
    let mut app = App::new(socket_path);
    app.refresh().await;

    // Main loop
    let tick_rate = Duration::from_millis(1000);
    let mut last_tick = std::time::Instant::now();

    loop {
        terminal.draw(|f| ui(f, &app))?;

        let timeout = tick_rate
            .checked_sub(last_tick.elapsed())
            .unwrap_or_else(|| Duration::from_secs(0));

        if crossterm::event::poll(timeout)? {
            if let Event::Key(key) = event::read()? {
                if key.kind == KeyEventKind::Press {
                    match key.code {
                        KeyCode::Char('q') | KeyCode::Esc => app.should_quit = true,
                        KeyCode::Down | KeyCode::Char('j') => app.next(),
                        KeyCode::Up | KeyCode::Char('k') => app.previous(),
                        KeyCode::Tab => app.next_tab(),
                        KeyCode::BackTab => app.previous_tab(),
                        KeyCode::Char('s') => app.stop_selected().await,
                        KeyCode::Char('r') => app.restart_selected().await,
                        KeyCode::Char('l') => {
                            app.refresh_logs().await;
                            app.tab_index = 2; // Switch to logs tab
                        }
                        _ => {}
                    }
                }
            }
        }

        if last_tick.elapsed() >= tick_rate {
            app.refresh().await;
            last_tick = std::time::Instant::now();
        }

        if app.should_quit {
            break;
        }
    }

    // Restore terminal
    disable_raw_mode()?;
    execute!(
        terminal.backend_mut(),
        LeaveAlternateScreen,
        DisableMouseCapture
    )?;
    terminal.show_cursor()?;

    Ok(())
}

fn ui(f: &mut Frame, app: &App) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .margin(1)
        .constraints([
            Constraint::Length(3),  // Tabs
            Constraint::Min(10),    // Main content
            Constraint::Length(3),  // Help bar
        ])
        .split(f.size());

    // Tabs
    let tab_titles = vec!["Processes", "Details", "Logs"];
    let tabs = Tabs::new(tab_titles)
        .block(Block::default().borders(Borders::ALL).title("OxidePM Monitor"))
        .select(app.tab_index)
        .style(Style::default().fg(Color::White))
        .highlight_style(Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD));
    f.render_widget(tabs, chunks[0]);

    // Main content based on selected tab
    match app.tab_index {
        0 => render_processes(f, app, chunks[1]),
        1 => render_details(f, app, chunks[1]),
        2 => render_logs(f, app, chunks[1]),
        _ => {}
    }

    // Help bar
    let help_text = match app.tab_index {
        0 => "↑/↓: Select | s: Stop | r: Restart | l: Logs | Tab: Switch | q: Quit",
        1 => "↑/↓: Select | Tab: Switch | q: Quit",
        2 => "↑/↓: Scroll | Tab: Switch | q: Quit",
        _ => "",
    };
    let help = Paragraph::new(help_text)
        .style(Style::default().fg(Color::DarkGray))
        .block(Block::default().borders(Borders::ALL));
    f.render_widget(help, chunks[2]);
}

fn render_processes(f: &mut Frame, app: &App, area: Rect) {
    let header_cells = ["ID", "Name", "Mode", "PID", "↺", "Status", "CPU", "Mem", "Uptime", "Port"]
        .iter()
        .map(|h| Cell::from(*h).style(Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD)));
    let header = Row::new(header_cells).height(1).bottom_margin(1);

    let rows: Vec<Row> = app.processes.iter().enumerate().map(|(i, info)| {
        let status_color = match info.state.status {
            AppStatus::Running => Color::Green,
            AppStatus::Stopped => Color::Red,
            AppStatus::Errored => Color::Red,
            AppStatus::Starting | AppStatus::Building => Color::Yellow,
            AppStatus::Stopping => Color::Yellow,
        };

        let cells = vec![
            Cell::from(info.spec.id.to_string()),
            Cell::from(info.spec.name.clone()),
            Cell::from(info.spec.mode.to_string()),
            Cell::from(info.state.pid.map(|p| p.to_string()).unwrap_or("-".to_string())),
            Cell::from(info.state.restarts.to_string()),
            Cell::from(info.state.status.as_str()).style(Style::default().fg(status_color)),
            Cell::from(format!("{:.1}%", info.state.cpu_percent)),
            Cell::from(format_bytes(info.state.memory_bytes)),
            Cell::from(format_duration(info.state.uptime_secs)),
            Cell::from(info.state.port.map(|p| p.to_string()).unwrap_or("-".to_string())),
        ];

        let style = if i == app.selected_index {
            Style::default().bg(Color::DarkGray)
        } else {
            Style::default()
        };

        Row::new(cells).style(style)
    }).collect();

    let table = Table::new(
        rows,
        [
            Constraint::Length(4),   // ID
            Constraint::Min(15),     // Name
            Constraint::Length(6),   // Mode
            Constraint::Length(7),   // PID
            Constraint::Length(3),   // Restarts
            Constraint::Length(10),  // Status
            Constraint::Length(7),   // CPU
            Constraint::Length(8),   // Mem
            Constraint::Length(8),   // Uptime
            Constraint::Length(6),   // Port
        ],
    )
    .header(header)
    .block(Block::default().borders(Borders::ALL).title("Processes"));

    f.render_widget(table, area);
}

fn render_details(f: &mut Frame, app: &App, area: Rect) {
    if app.processes.is_empty() {
        let paragraph = Paragraph::new("No processes")
            .block(Block::default().borders(Borders::ALL).title("Details"));
        f.render_widget(paragraph, area);
        return;
    }

    let info = &app.processes[app.selected_index];

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(8),  // Info
            Constraint::Length(4),  // CPU gauge
            Constraint::Length(4),  // Memory gauge
            Constraint::Min(5),     // Environment
        ])
        .split(area);

    // Basic info
    let info_text = vec![
        Line::from(vec![
            Span::raw("Name: "),
            Span::styled(&info.spec.name, Style::default().fg(Color::Cyan)),
        ]),
        Line::from(vec![
            Span::raw("ID: "),
            Span::styled(info.spec.id.to_string(), Style::default().fg(Color::Yellow)),
        ]),
        Line::from(vec![
            Span::raw("Mode: "),
            Span::raw(info.spec.mode.to_string()),
        ]),
        Line::from(vec![
            Span::raw("Command: "),
            Span::raw(&info.spec.command),
        ]),
        Line::from(vec![
            Span::raw("CWD: "),
            Span::raw(info.spec.cwd.display().to_string()),
        ]),
        Line::from(vec![
            Span::raw("Instances: "),
            Span::raw(info.spec.instances.to_string()),
        ]),
    ];
    let info_paragraph = Paragraph::new(info_text)
        .block(Block::default().borders(Borders::ALL).title("Info"));
    f.render_widget(info_paragraph, chunks[0]);

    // CPU gauge
    let cpu_gauge = Gauge::default()
        .block(Block::default().borders(Borders::ALL).title("CPU"))
        .gauge_style(Style::default().fg(Color::Green))
        .percent((info.state.cpu_percent.min(100.0)) as u16)
        .label(format!("{:.1}%", info.state.cpu_percent));
    f.render_widget(cpu_gauge, chunks[1]);

    // Memory gauge (assume 1GB max for display)
    let mem_percent = ((info.state.memory_bytes as f64 / (1024.0 * 1024.0 * 1024.0)) * 100.0).min(100.0) as u16;
    let mem_gauge = Gauge::default()
        .block(Block::default().borders(Borders::ALL).title("Memory"))
        .gauge_style(Style::default().fg(Color::Blue))
        .percent(mem_percent)
        .label(format_bytes(info.state.memory_bytes));
    f.render_widget(mem_gauge, chunks[2]);

    // Environment
    let env_text: Vec<Line> = info.spec.env.iter()
        .take(10)
        .map(|(k, v)| Line::from(format!("{}={}", k, v)))
        .collect();
    let env_paragraph = Paragraph::new(env_text)
        .block(Block::default().borders(Borders::ALL).title("Environment"));
    f.render_widget(env_paragraph, chunks[3]);
}

fn render_logs(f: &mut Frame, app: &App, area: Rect) {
    let logs_text: Vec<Line> = app.logs.iter()
        .rev()
        .take(area.height as usize - 2)
        .rev()
        .map(|line| Line::from(line.as_str()))
        .collect();

    let title = if app.processes.is_empty() {
        "Logs".to_string()
    } else {
        format!("Logs - {}", app.processes[app.selected_index].spec.name)
    };

    let paragraph = Paragraph::new(logs_text)
        .block(Block::default().borders(Borders::ALL).title(title));
    f.render_widget(paragraph, area);
}

fn format_bytes(bytes: u64) -> String {
    if bytes >= 1_073_741_824 {
        format!("{:.1}G", bytes as f64 / 1_073_741_824.0)
    } else if bytes >= 1_048_576 {
        format!("{:.1}M", bytes as f64 / 1_048_576.0)
    } else if bytes >= 1024 {
        format!("{:.0}K", bytes as f64 / 1024.0)
    } else {
        format!("{}B", bytes)
    }
}

fn format_duration(secs: u64) -> String {
    if secs >= 86400 {
        format!("{}d", secs / 86400)
    } else if secs >= 3600 {
        format!("{}h", secs / 3600)
    } else if secs >= 60 {
        format!("{}m", secs / 60)
    } else {
        format!("{}s", secs)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_format_bytes() {
        assert_eq!(format_bytes(500), "500B");
        assert_eq!(format_bytes(2048), "2K");
        assert_eq!(format_bytes(1_500_000), "1.4M");
        assert_eq!(format_bytes(2_000_000_000), "1.9G");
    }

    #[test]
    fn test_format_duration() {
        assert_eq!(format_duration(30), "30s");
        assert_eq!(format_duration(90), "1m");
        assert_eq!(format_duration(3700), "1h");
        assert_eq!(format_duration(90000), "1d");
    }
}
