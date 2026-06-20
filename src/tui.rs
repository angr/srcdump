//! Interactive TUI browser for decompiled functions.
//!
//! When stdout is a terminal, srcdump runs this instead of dumping every
//! function. The worker pool runs on a background thread, streaming results
//! into a channel; the main thread drives input and rendering. Each function is
//! shown with a "waiting for result" banner until its decompilation arrives.
//!
//! Keys: `,`/`.` switch function, `j`/`k` move the highlighted line, `q`/`Esc`
//! quit.

use crate::backend::DecompileRequest;
use crate::model::{FunctionInfo, WorkerMessage};
use crate::pool::{Cancellation, ProcessPoolBackend};
use anyhow::{Context, Result};
use ratatui::backend::CrosstermBackend;
use ratatui::crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
use ratatui::crossterm::{execute, terminal};
use ratatui::layout::{Alignment, Constraint, Direction, Layout};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::Line;
use ratatui::widgets::{Block, Borders, List, ListItem, ListState, Paragraph};
use ratatui::{Frame, Terminal};
use std::collections::BTreeMap;
use std::io::{self, Stdout};
use std::path::PathBuf;
use std::sync::mpsc::{self, TryRecvError};
use std::sync::Arc;
use std::thread;
use std::time::Duration;

/// Decompilation state of one function.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Status {
    Pending,
    Done,
    Failed,
}

struct Entry {
    info: FunctionInfo,
    lines: Vec<String>,
    error: Option<String>,
    status: Status,
}

impl Entry {
    fn pending(info: FunctionInfo) -> Self {
        Self {
            info,
            lines: Vec::new(),
            error: None,
            status: Status::Pending,
        }
    }
}

struct App {
    file: PathBuf,
    file_format: Option<String>,
    entries: BTreeMap<u64, Entry>,
    order: Vec<u64>,
    sel: usize,
    line: usize,
    scroll: usize,
    build_status: String,
    done: bool,
    done_count: usize,
    total: usize,
    fatal: Option<String>,
}

impl App {
    fn new(file: PathBuf) -> Self {
        Self {
            file,
            file_format: None,
            entries: BTreeMap::new(),
            order: Vec::new(),
            sel: 0,
            line: 0,
            scroll: 0,
            build_status: "Starting…".to_string(),
            done: false,
            done_count: 0,
            total: 0,
            fatal: None,
        }
    }

    fn apply(&mut self, msg: WorkerMessage) {
        match msg {
            WorkerMessage::Progress { stage, percent } => {
                self.build_status = match stage.as_str() {
                    "load" => "Loading binary…".to_string(),
                    "cfg" => format!("Building CFG… {}%", percent.unwrap_or(0.0) as u32),
                    "save" => "Saving CFG cache…".to_string(),
                    _ => std::mem::take(&mut self.build_status),
                };
            }
            WorkerMessage::Meta { file_format } => {
                self.file_format.get_or_insert(file_format);
            }
            WorkerMessage::Catalog { functions } => {
                for f in functions {
                    self.entries.entry(f.addr).or_insert_with(|| Entry::pending(f));
                }
                self.rebuild_order();
            }
            WorkerMessage::Plan { count } => self.total += count,
            WorkerMessage::Function(f) => {
                self.done_count += 1;
                let addr = f.addr;
                let lines = f
                    .text
                    .as_deref()
                    .unwrap_or("")
                    .lines()
                    .map(str::to_string)
                    .collect();
                let known = self.entries.contains_key(&addr);
                let entry = self
                    .entries
                    .entry(addr)
                    .or_insert_with(|| Entry::pending(f.clone()));
                entry.info = f;
                entry.lines = lines;
                entry.status = Status::Done;
                entry.error = None;
                if !known {
                    self.rebuild_order();
                }
            }
            WorkerMessage::Error(err) => {
                self.done_count += 1;
                let addr = err.addr;
                let known = self.entries.contains_key(&addr);
                let entry = self.entries.entry(addr).or_insert_with(|| {
                    Entry::pending(FunctionInfo {
                        addr,
                        name: err.name.clone(),
                        size: 0,
                        is_plt: false,
                        section: None,
                        text: None,
                        decompile_ms: None,
                    })
                });
                entry.error = Some(err.message);
                entry.status = Status::Failed;
                if !known {
                    self.rebuild_order();
                }
            }
            WorkerMessage::Fatal { message } => self.fatal = Some(message),
        }
    }

    /// Recompute the sorted address order, keeping the same function selected.
    fn rebuild_order(&mut self) {
        let sel_addr = self.order.get(self.sel).copied();
        self.order = self.entries.keys().copied().collect();
        if let Some(a) = sel_addr {
            if let Some(pos) = self.order.iter().position(|&x| x == a) {
                self.sel = pos;
            }
        }
        if self.sel >= self.order.len() {
            self.sel = self.order.len().saturating_sub(1);
        }
    }

    fn current(&self) -> Option<&Entry> {
        self.order.get(self.sel).and_then(|a| self.entries.get(a))
    }

    fn select_next(&mut self) {
        if self.sel + 1 < self.order.len() {
            self.sel += 1;
            self.line = 0;
            self.scroll = 0;
        }
    }

    fn select_prev(&mut self) {
        if self.sel > 0 {
            self.sel -= 1;
            self.line = 0;
            self.scroll = 0;
        }
    }

    fn move_line(&mut self, delta: isize) {
        let n = self.current().map(|e| e.lines.len()).unwrap_or(0);
        if n == 0 {
            return;
        }
        self.line = (self.line as isize + delta).clamp(0, n as isize - 1) as usize;
    }
}

/// Run the interactive browser. Takes ownership of the pool so it can be shared
/// with the background worker thread.
pub fn run(backend: ProcessPoolBackend, req: DecompileRequest, file: PathBuf) -> Result<()> {
    let (tx, rx) = mpsc::channel::<WorkerMessage>();
    let backend = Arc::new(backend);
    let cancel = Arc::new(Cancellation::default());

    let bg_backend = Arc::clone(&backend);
    let bg_cancel = Arc::clone(&cancel);
    let bg_req = req.clone();
    let worker = thread::spawn(move || {
        let r = bg_backend.run_streaming(&bg_req, &bg_cancel, |m| {
            let _ = tx.send(m);
        });
        if let Err(e) = r {
            let _ = tx.send(WorkerMessage::Fatal {
                message: format!("{e:#}"),
            });
        }
        // `tx` drops here, disconnecting the channel so the UI knows we're done.
    });

    let mut terminal = setup_terminal().context("entering TUI")?;
    let _guard = TerminalGuard;

    let mut app = App::new(file);
    let loop_result = event_loop(&mut terminal, &mut app, &rx);

    // Tear the workers down so none linger decompiling after we exit.
    cancel.cancel();
    let _ = worker.join();
    loop_result
}

fn setup_terminal() -> Result<Terminal<CrosstermBackend<Stdout>>> {
    terminal::enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, terminal::EnterAlternateScreen)?;
    Ok(Terminal::new(CrosstermBackend::new(stdout))?)
}

/// Restores the terminal on drop, including during a panic.
struct TerminalGuard;
impl Drop for TerminalGuard {
    fn drop(&mut self) {
        let _ = terminal::disable_raw_mode();
        let _ = execute!(io::stdout(), terminal::LeaveAlternateScreen);
    }
}

fn event_loop(
    terminal: &mut Terminal<CrosstermBackend<Stdout>>,
    app: &mut App,
    rx: &mpsc::Receiver<WorkerMessage>,
) -> Result<()> {
    loop {
        // Drain everything available without blocking, then redraw.
        loop {
            match rx.try_recv() {
                Ok(msg) => app.apply(msg),
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => {
                    app.done = true;
                    break;
                }
            }
        }

        terminal.draw(|f| render(f, app))?;

        if event::poll(Duration::from_millis(100))? {
            if let Event::Key(key) = event::read()? {
                if key.kind != KeyEventKind::Press {
                    continue;
                }
                match (key.code, key.modifiers) {
                    (KeyCode::Char('q'), _) | (KeyCode::Esc, _) => return Ok(()),
                    (KeyCode::Char('c'), KeyModifiers::CONTROL) => return Ok(()),
                    (KeyCode::Char(','), _) => app.select_prev(),
                    (KeyCode::Char('.'), _) => app.select_next(),
                    (KeyCode::Char('j'), _) | (KeyCode::Down, _) => app.move_line(1),
                    (KeyCode::Char('k'), _) | (KeyCode::Up, _) => app.move_line(-1),
                    _ => {}
                }
            }
        }
    }
}

fn render(f: &mut Frame, app: &mut App) {
    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),
            Constraint::Min(1),
            Constraint::Length(1),
        ])
        .split(f.area());
    let (header, body, footer) = (rows[0], rows[1], rows[2]);

    // Header.
    let fmt = app.file_format.as_deref().unwrap_or("…");
    let pos = if app.order.is_empty() {
        "0/0".to_string()
    } else {
        format!("{}/{}", app.sel + 1, app.order.len())
    };
    let status = if app.done {
        format!("done · {} functions", app.done_count)
    } else {
        format!("{}/{} decompiled", app.done_count, app.total)
    };
    let header_line = format!(
        " srcdump  {}  {}   [{}]   {}",
        app.file.display(),
        fmt,
        pos,
        status
    );
    f.render_widget(
        Paragraph::new(header_line).style(Style::default().add_modifier(Modifier::BOLD)),
        header,
    );

    // Body: sidebar + main pane.
    let cols = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Length(36), Constraint::Min(1)])
        .split(body);
    render_sidebar(f, app, cols[0]);
    render_main(f, app, cols[1]);

    // Footer hints.
    f.render_widget(
        Paragraph::new(" ,/. switch function   j/k move line   q quit")
            .style(Style::default().fg(Color::DarkGray)),
        footer,
    );
}

fn render_sidebar(f: &mut Frame, app: &App, area: ratatui::layout::Rect) {
    let items: Vec<ListItem> = app
        .order
        .iter()
        .map(|addr| {
            let e = &app.entries[addr];
            let (glyph, color) = match e.status {
                Status::Pending => ('·', Color::DarkGray),
                Status::Done => ('✓', Color::Green),
                Status::Failed => ('✗', Color::Red),
            };
            ListItem::new(format!("{glyph} 0x{:x} {}", addr, e.info.name))
                .style(Style::default().fg(color))
        })
        .collect();

    let list = List::new(items)
        .block(Block::new().borders(Borders::ALL).title("Functions"))
        .highlight_style(Style::default().add_modifier(Modifier::REVERSED))
        .highlight_symbol("▶ ");
    let mut state = ListState::default();
    if !app.order.is_empty() {
        state.select(Some(app.sel));
    }
    f.render_stateful_widget(list, area, &mut state);
}

fn render_main(f: &mut Frame, app: &mut App, area: ratatui::layout::Rect) {
    let Some(addr) = app.order.get(app.sel).copied() else {
        // Nothing enumerated yet — show the build/loading status.
        let block = Block::new().borders(Borders::ALL).title("decompilation");
        let msg = app.fatal.clone().unwrap_or_else(|| app.build_status.clone());
        f.render_widget(
            Paragraph::new(format!("\n  {msg}")).block(block),
            area,
        );
        return;
    };

    let entry = &app.entries[&addr];
    let mut title = format!("0x{:x} <{}>", addr, entry.info.name);
    if let Some(sec) = &entry.info.section {
        title.push_str(&format!("  {sec}"));
    }
    if let Some(ms) = entry.info.decompile_ms {
        title.push_str(&format!("  {ms:.0} ms"));
    }
    let block = Block::new().borders(Borders::ALL).title(title);

    match entry.status {
        Status::Pending => {
            f.render_widget(
                Paragraph::new("\n\n  ⏳ waiting for result…")
                    .alignment(Alignment::Center)
                    .style(Style::default().fg(Color::Yellow))
                    .block(block),
                area,
            );
        }
        Status::Failed => {
            let msg = entry.error.as_deref().unwrap_or("decompilation failed");
            f.render_widget(
                Paragraph::new(format!("\n  ✗ {msg}"))
                    .style(Style::default().fg(Color::Red))
                    .block(block),
                area,
            );
        }
        Status::Done => {
            // Keep the highlighted line within the viewport.
            let view_h = area.height.saturating_sub(2) as usize;
            let n = entry.lines.len();
            if app.line >= n {
                app.line = n.saturating_sub(1);
            }
            if app.line < app.scroll {
                app.scroll = app.line;
            } else if view_h > 0 && app.line >= app.scroll + view_h {
                app.scroll = app.line + 1 - view_h;
            }

            let entry = &app.entries[&addr];
            let lines: Vec<Line> = entry
                .lines
                .iter()
                .enumerate()
                .map(|(i, text)| {
                    let style = if i == app.line {
                        Style::default().add_modifier(Modifier::REVERSED)
                    } else {
                        Style::default()
                    };
                    Line::styled(text.clone(), style)
                })
                .collect();
            f.render_widget(
                Paragraph::new(lines)
                    .scroll((app.scroll as u16, 0))
                    .block(block),
                area,
            );
        }
    }
}
