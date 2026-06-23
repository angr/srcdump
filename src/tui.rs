//! Interactive TUI browser for decompiled functions.
//!
//! When stdout is a terminal, srcdump runs this instead of dumping every
//! function. The worker pool runs on a background thread, streaming results
//! into a channel; the main thread drives input and rendering.
//!
//! Functions are shown as one continuous document: reaching the bottom of a
//! function flows straight into the next. Each function shows a "waiting for
//! result" banner until its decompilation arrives.
//!
//! Keys: `j`/`k` move the highlighted line (crossing function boundaries),
//! `PgUp`/`PgDn` scroll by a page, `.`/`,` jump to the next/previous function,
//! `f` toggles the function panel, `h` toggles help, `q`/`Esc` quits.

use crate::backend::DecompileRequest;
use crate::highlight::{self, Tok, TokKind};
use crate::model::{FunctionInfo, WorkerMessage};
use crate::pool::{Cancellation, ProcessPoolBackend};
use anyhow::{Context, Result};
use ratatui::backend::CrosstermBackend;
use ratatui::crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
use ratatui::crossterm::{execute, terminal};
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, List, ListItem, ListState, Paragraph};
use ratatui::{Frame, Terminal};
use std::collections::BTreeMap;
use std::io::{self, Stdout};
use std::path::PathBuf;
use std::sync::mpsc::{self, TryRecvError};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

/// Background for the highlighted cursor line (a dark gray in the 256-color
/// palette, so token foreground colors stay readable on top).
const CURSOR_BG: Color = Color::Indexed(237);

/// Foreground style for a highlight token kind.
fn style_for(kind: TokKind) -> Style {
    let st = Style::default();
    match kind {
        TokKind::Keyword => st.fg(Color::Magenta).add_modifier(Modifier::BOLD),
        TokKind::Type => st.fg(Color::Cyan),
        TokKind::Call => st.fg(Color::LightBlue),
        TokKind::Number => st.fg(Color::Yellow),
        TokKind::Str => st.fg(Color::Green),
        TokKind::Comment => st.fg(Color::DarkGray),
        TokKind::Field | TokKind::Var | TokKind::Plain => st,
    }
}

/// Decompilation state of one function.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Status {
    Pending,
    Done,
    Failed,
}

struct Entry {
    info: FunctionInfo,
    /// Highlighted code, one token-run vector per line (empty until Done).
    lines: Vec<Vec<Tok>>,
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

    /// Number of document lines this function occupies: a header line, its body
    /// (the code, or a one-line banner while pending/failed), and one blank
    /// separator line after it.
    fn block_len(&self) -> usize {
        2 + match self.status {
            Status::Done => self.lines.len().max(1),
            _ => 1,
        }
    }
}

struct App {
    file: PathBuf,
    file_format: Option<String>,
    entries: BTreeMap<u64, Entry>,
    order: Vec<u64>,

    // Cursor: function `sel` (index into `order`) and line offset within its
    // block (0 == the function header line). This logical anchor stays stable
    // as functions stream in and blocks grow.
    sel: usize,
    off: usize,
    scroll: usize,
    /// Height of the doc viewport, updated each render (for Page Up/Down).
    page: usize,

    show_sidebar: bool,
    show_help: bool,
    show_times: bool,

    build_status: String,
    done: bool,
    done_count: usize,
    total: usize,
    fatal: Option<String>,

    // Cheap layout cache: `starts[i]` is the first document line of `order[i]`.
    starts: Vec<usize>,
    total_lines: usize,
    dirty: bool,

    // Available-memory sample for the status bar, refreshed every 0.5s.
    mem_avail: Option<u64>,
    mem_checked: Option<Instant>,
}

impl App {
    fn new(file: PathBuf) -> Self {
        Self {
            file,
            file_format: None,
            entries: BTreeMap::new(),
            order: Vec::new(),
            sel: 0,
            off: 0,
            scroll: 0,
            page: 0,
            show_sidebar: true,
            show_help: false,
            show_times: false,
            build_status: "Starting…".to_string(),
            done: false,
            done_count: 0,
            total: 0,
            fatal: None,
            starts: Vec::new(),
            total_lines: 0,
            dirty: true,
            mem_avail: None,
            mem_checked: None,
        }
    }

    /// Re-sample available memory at most every 0.5 seconds.
    fn refresh_mem(&mut self, now: Instant) {
        let stale = self
            .mem_checked
            .is_none_or(|t| now.duration_since(t) >= Duration::from_millis(500));
        if stale {
            self.mem_avail = read_mem_available();
            self.mem_checked = Some(now);
        }
    }

    /// Cursor position as percentages: within the current function, and across
    /// the whole document.
    fn cursor_percents(&self) -> (u32, u32) {
        if self.order.is_empty() {
            return (0, 0);
        }
        let nl = self.nav_len(self.sel);
        let off = self.off.min(nl.saturating_sub(1));
        let func = if nl <= 1 {
            100
        } else {
            (off as u64 * 100 / (nl as u64 - 1)) as u32
        };
        let overall = if self.total_lines <= 1 {
            100
        } else {
            (self.cursor_line() as u64 * 100 / (self.total_lines as u64 - 1)) as u32
        };
        (func, overall)
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
                let lines = highlight::highlight_lines(f.text.as_deref().unwrap_or(""), &f.spans);
                let known = self.entries.contains_key(&addr);
                let entry = self
                    .entries
                    .entry(addr)
                    .or_insert_with(|| Entry::pending(f.clone()));
                entry.info = f;
                entry.lines = lines;
                entry.status = Status::Done;
                entry.error = None;
                if known {
                    self.dirty = true;
                } else {
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
                        spans: Vec::new(),
                    })
                });
                entry.error = Some(err.message);
                entry.status = Status::Failed;
                if known {
                    self.dirty = true;
                } else {
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
        self.dirty = true;
    }

    /// Rebuild the line-offset cache if functions changed.
    fn ensure_layout(&mut self) {
        if !self.dirty {
            return;
        }
        self.dirty = false;
        self.starts.clear();
        let mut acc = 0;
        for &addr in &self.order {
            self.starts.push(acc);
            acc += self.entries[&addr].block_len();
        }
        self.total_lines = acc;
        if self.sel >= self.order.len() {
            self.sel = self.order.len().saturating_sub(1);
        }
    }

    fn block_len(&self, i: usize) -> usize {
        self.entries[&self.order[i]].block_len()
    }

    /// Number of cursor-navigable lines in a block: everything but the trailing
    /// blank separator, which renders but can't be highlighted.
    fn nav_len(&self, i: usize) -> usize {
        self.block_len(i).saturating_sub(1)
    }

    /// Absolute document line index of the cursor.
    fn cursor_line(&self) -> usize {
        if self.order.is_empty() {
            return 0;
        }
        let off = self.off.min(self.nav_len(self.sel).saturating_sub(1));
        self.starts[self.sel] + off
    }

    fn move_down(&mut self) {
        if self.order.is_empty() {
            return;
        }
        let nl = self.nav_len(self.sel);
        if self.off + 1 < nl {
            self.off += 1;
        } else if self.sel + 1 < self.order.len() {
            self.sel += 1;
            self.off = 0;
        }
    }

    fn move_up(&mut self) {
        if self.order.is_empty() {
            return;
        }
        // Snap a possibly-clamped offset to a concrete value first.
        self.off = self.off.min(self.nav_len(self.sel).saturating_sub(1));
        if self.off > 0 {
            self.off -= 1;
        } else if self.sel > 0 {
            self.sel -= 1;
            self.off = self.nav_len(self.sel).saturating_sub(1);
        }
    }

    fn next_func(&mut self) {
        if self.sel + 1 < self.order.len() {
            self.sel += 1;
            self.off = 0;
        }
    }

    fn prev_func(&mut self) {
        if self.sel > 0 {
            self.sel -= 1;
            self.off = 0;
        }
    }

    fn page_down(&mut self) {
        if self.order.is_empty() {
            return;
        }
        let step = self.page.max(1);
        let target = (self.cursor_line() + step).min(self.total_lines.saturating_sub(1));
        self.set_cursor_to_line(target);
    }

    fn page_up(&mut self) {
        if self.order.is_empty() {
            return;
        }
        let step = self.page.max(1);
        let target = self.cursor_line().saturating_sub(step);
        self.set_cursor_to_line(target);
    }

    /// Move the cursor to an absolute document line, snapping off any trailing
    /// blank separator so the highlight lands on a real line.
    fn set_cursor_to_line(&mut self, abs: usize) {
        let (i, off) = self.locate(abs);
        self.sel = i;
        self.off = off.min(self.nav_len(i).saturating_sub(1));
    }

    /// Build the fully-styled document line for function `i` at block-offset
    /// `off`. When `cursor`, every span gets a highlight background.
    fn doc_line(&self, i: usize, off: usize, cursor: bool) -> Line<'static> {
        let entry = &self.entries[&self.order[i]];
        let bg = |st: Style| if cursor { st.bg(CURSOR_BG) } else { st };

        if off == 0 {
            let mut h = format!("// 0x{:x} <{}>", entry.info.addr, entry.info.name);
            if let Some(sec) = &entry.info.section {
                h.push_str(&format!("  {sec}"));
            }
            if self.show_times {
                if let Some(ms) = entry.info.decompile_ms {
                    h.push_str(&format!("  {ms:.0} ms"));
                }
            }
            let style = Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD);
            return Line::from(Span::styled(h, bg(style)));
        }

        let body = off - 1;
        match entry.status {
            // For `Done`, `get` returns `None` past the last code line — that's
            // the trailing blank separator.
            Status::Done => match entry.lines.get(body) {
                Some(toks) if !toks.is_empty() => Line::from(
                    toks.iter()
                        .map(|t| Span::styled(t.text.clone(), bg(style_for(t.kind))))
                        .collect::<Vec<_>>(),
                ),
                _ => Line::from(Span::styled(String::new(), bg(Style::default()))),
            },
            // Pending/failed bodies are a single line; anything after is blank.
            Status::Pending if body == 0 => Line::from(Span::styled(
                "    Decompiling…".to_string(),
                bg(Style::default().fg(Color::Yellow)),
            )),
            Status::Failed if body == 0 => Line::from(Span::styled(
                format!(
                    "    ✗ {}",
                    entry.error.as_deref().unwrap_or("decompilation failed")
                ),
                bg(Style::default().fg(Color::Red)),
            )),
            _ => Line::from(Span::styled(String::new(), bg(Style::default()))),
        }
    }

    /// Find (function index, block offset) for an absolute document line.
    fn locate(&self, abs: usize) -> (usize, usize) {
        let i = self.starts.partition_point(|&s| s <= abs).saturating_sub(1);
        (i, abs - self.starts[i])
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

/// Run the browser over an already-complete result (warm cache), without any
/// Python workers. The result is fed into the app as if it had streamed in.
pub fn run_cached(result: crate::backend::DecompileResult, file: PathBuf) -> Result<()> {
    let mut terminal = setup_terminal().context("entering TUI")?;
    let _guard = TerminalGuard;

    let mut app = App::new(file);
    if let Some(fmt) = result.file_format {
        app.apply(WorkerMessage::Meta { file_format: fmt });
    }
    let total = result.functions.len();
    for func in result.functions {
        app.apply(WorkerMessage::Function(func));
    }
    app.total = total;
    app.done = true;

    // A pre-closed channel: the event loop just renders and handles input.
    let (tx, rx) = mpsc::channel::<WorkerMessage>();
    drop(tx);
    event_loop(&mut terminal, &mut app, &rx)
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

        app.ensure_layout();
        app.refresh_mem(Instant::now());
        terminal.draw(|f| render(f, app))?;

        if event::poll(Duration::from_millis(100))? {
            if let Event::Key(key) = event::read()? {
                if key.kind != KeyEventKind::Press {
                    continue;
                }
                app.ensure_layout();
                match (key.code, key.modifiers) {
                    (KeyCode::Char('q'), _) | (KeyCode::Esc, _) => return Ok(()),
                    (KeyCode::Char('c'), KeyModifiers::CONTROL) => return Ok(()),
                    (KeyCode::Char('h'), _) => app.show_help = !app.show_help,
                    (KeyCode::Char('f'), _) => app.show_sidebar = !app.show_sidebar,
                    (KeyCode::Char('t'), _) => app.show_times = !app.show_times,
                    (KeyCode::Char('.'), _) => app.next_func(),
                    (KeyCode::Char(','), _) => app.prev_func(),
                    (KeyCode::Char('j'), _) | (KeyCode::Down, _) => app.move_down(),
                    (KeyCode::Char('k'), _) | (KeyCode::Up, _) => app.move_up(),
                    (KeyCode::PageDown, _) => app.page_down(),
                    (KeyCode::PageUp, _) => app.page_up(),
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
            Constraint::Length(1),
        ])
        .split(f.area());
    let (header, body, status, footer) = (rows[0], rows[1], rows[2], rows[3]);

    render_header(f, app, header);

    if app.show_sidebar {
        let cols = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Length(36), Constraint::Min(1)])
            .split(body);
        render_sidebar(f, app, cols[0]);
        render_doc(f, app, cols[1]);
    } else {
        render_doc(f, app, body);
    }

    render_status(f, app, status);

    f.render_widget(
        Paragraph::new(" j/k line   PgUp/PgDn page   ./, function   f panel   t times   h help   q quit")
            .style(Style::default().fg(Color::DarkGray)),
        footer,
    );

    if app.show_help {
        render_help(f);
    }
}

/// Bottom status bar: available memory plus cursor position percentages.
fn render_status(f: &mut Frame, app: &App, area: Rect) {
    let mem = match app.mem_avail {
        Some(b) => format!("{} free", human_bytes(b)),
        None => "n/a".to_string(),
    };
    let (func_pct, overall_pct) = app.cursor_percents();
    let left = format!(" mem {mem}");
    let right = format!("{func_pct}% func / {overall_pct}% overall ");

    let width = area.width as usize;
    let pad = width.saturating_sub(left.chars().count() + right.chars().count());
    let line = format!("{left}{}{right}", " ".repeat(pad));

    f.render_widget(
        Paragraph::new(line).style(Style::default().bg(Color::Indexed(236)).fg(Color::Gray)),
        area,
    );
}

fn render_header(f: &mut Frame, app: &App, area: Rect) {
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
    let line = format!(
        " srcdump  {}  {}   [{}]   {}",
        app.file.display(),
        fmt,
        pos,
        status
    );
    f.render_widget(
        Paragraph::new(line).style(Style::default().add_modifier(Modifier::BOLD)),
        area,
    );
}

fn render_sidebar(f: &mut Frame, app: &App, area: Rect) {
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

fn render_doc(f: &mut Frame, app: &mut App, area: Rect) {
    let block = Block::new().borders(Borders::ALL).title("decompilation");

    if app.order.is_empty() {
        let msg = app.fatal.clone().unwrap_or_else(|| app.build_status.clone());
        f.render_widget(Paragraph::new(format!("\n  {msg}")).block(block), area);
        return;
    }

    let view_h = area.height.saturating_sub(2) as usize;
    app.page = view_h;
    let cursor = app.cursor_line();

    // Keep the cursor within the viewport.
    if cursor < app.scroll {
        app.scroll = cursor;
    } else if view_h > 0 && cursor >= app.scroll + view_h {
        app.scroll = cursor + 1 - view_h;
    }
    if app.scroll > app.total_lines.saturating_sub(1) {
        app.scroll = app.total_lines.saturating_sub(1);
    }

    let mut lines: Vec<Line> = Vec::with_capacity(view_h);
    for abs in app.scroll..(app.scroll + view_h).min(app.total_lines) {
        let (i, off) = app.locate(abs);
        lines.push(app.doc_line(i, off, abs == cursor));
    }

    f.render_widget(Paragraph::new(lines).block(block), area);
}

fn render_help(f: &mut Frame) {
    let area = centered_rect(60, 11, f.area());
    let text = "\n  j / k         move the highlighted line (crosses functions)\n  PgUp / PgDn   scroll by a page\n  . / ,         jump to next / previous function\n  f             toggle the function panel\n  t             toggle decompilation times\n  h             toggle this help\n  q / Esc       quit";
    f.render_widget(Clear, area);
    f.render_widget(
        Paragraph::new(text).block(
            Block::new()
                .borders(Borders::ALL)
                .title("Help — keys")
                .style(Style::default().fg(Color::White)),
        ),
        area,
    );
}

/// A rectangle `width` columns wide and `height` rows tall, centered in `area`.
fn centered_rect(width: u16, height: u16, area: Rect) -> Rect {
    let w = width.min(area.width);
    let h = height.min(area.height);
    let x = area.x + (area.width.saturating_sub(w)) / 2;
    let y = area.y + (area.height.saturating_sub(h)) / 2;
    Rect::new(x, y, w, h)
}

/// Available system memory in bytes, from `/proc/meminfo` (Linux). `None`
/// elsewhere or on parse failure.
fn read_mem_available() -> Option<u64> {
    let meminfo = std::fs::read_to_string("/proc/meminfo").ok()?;
    for line in meminfo.lines() {
        if let Some(rest) = line.strip_prefix("MemAvailable:") {
            let kb: u64 = rest.split_whitespace().next()?.parse().ok()?;
            return Some(kb * 1024);
        }
    }
    None
}

/// Format a byte count as `GiB`/`MiB`.
fn human_bytes(bytes: u64) -> String {
    let gib = bytes as f64 / (1024.0 * 1024.0 * 1024.0);
    if gib >= 1.0 {
        format!("{gib:.1} GiB")
    } else {
        format!("{:.0} MiB", bytes as f64 / (1024.0 * 1024.0))
    }
}
