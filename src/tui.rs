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
//! `g` go to a function by name or address, `/` full-text search (`F3`/`F4`
//! next/prev match), `f` toggles the function panel, `h` toggles help, `q`/`Esc`
//! quits.

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
        TokKind::Comment => st.fg(Color::Indexed(245)),
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
    angr_version: Option<String>,
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
    // The "go to function" dialog: the current input, and whether the last
    // submission found no match.
    goto: Option<String>,
    goto_failed: bool,
    // Full-text search: the dialog input (when open), the active query for
    // F3/F4, and whether the last search found nothing.
    search: Option<String>,
    search_query: Option<String>,
    search_failed: bool,

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
            angr_version: None,
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
            goto: None,
            goto_failed: false,
            search: None,
            search_query: None,
            search_failed: false,
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

    fn open_goto(&mut self) {
        self.goto = Some(String::new());
        self.goto_failed = false;
    }

    fn goto_push(&mut self, c: char) {
        if let Some(buf) = self.goto.as_mut() {
            buf.push(c);
        }
        self.goto_failed = false;
    }

    fn goto_backspace(&mut self) {
        if let Some(buf) = self.goto.as_mut() {
            buf.pop();
        }
        self.goto_failed = false;
    }

    /// Resolve the goto query and jump to the function, or flag "not found".
    fn goto_submit(&mut self) {
        let query = self.goto.clone().unwrap_or_default();
        let funcs: Vec<(u64, String, u64)> = self
            .order
            .iter()
            .map(|a| {
                let e = &self.entries[a];
                (*a, e.info.name.clone(), e.info.size)
            })
            .collect();
        match resolve_goto(&query, &funcs) {
            Some(idx) => {
                self.sel = idx;
                self.off = 0;
                self.goto = None;
                self.goto_failed = false;
            }
            None => self.goto_failed = true,
        }
    }

    fn open_search(&mut self) {
        self.search = Some(String::new());
        self.search_failed = false;
    }

    fn search_push(&mut self, c: char) {
        if let Some(buf) = self.search.as_mut() {
            buf.push(c);
        }
        self.search_failed = false;
    }

    fn search_backspace(&mut self) {
        if let Some(buf) = self.search.as_mut() {
            buf.pop();
        }
        self.search_failed = false;
    }

    /// Run the typed query: jump to the first match at or after the cursor.
    fn search_submit(&mut self) {
        let query = self.search.clone().unwrap_or_default();
        if query.trim().is_empty() {
            self.search = None;
            return;
        }
        self.search_query = Some(query.clone());
        let needle = query.to_lowercase();
        let start = self.cursor_line();
        match find_match(self.total_lines, &needle, start, true, |abs| {
            self.doc_line_text(abs)
        }) {
            Some(abs) => {
                self.set_cursor_to_line(abs);
                self.search = None;
                self.search_failed = false;
            }
            None => self.search_failed = true,
        }
    }

    /// Continue the active search (F3 forward, F4 backward).
    fn search_next(&mut self, forward: bool) {
        let Some(query) = self.search_query.clone() else {
            return;
        };
        if self.total_lines == 0 {
            return;
        }
        let needle = query.to_lowercase();
        let n = self.total_lines;
        let cur = self.cursor_line();
        let start = if forward {
            (cur + 1) % n
        } else {
            (cur + n - 1) % n
        };
        if let Some(abs) = find_match(n, &needle, start, forward, |abs| self.doc_line_text(abs)) {
            self.set_cursor_to_line(abs);
        }
    }

    /// Plain (unstyled) text of one document line, for searching.
    fn doc_line_text(&self, abs: usize) -> String {
        let (i, off) = self.locate(abs);
        let entry = &self.entries[&self.order[i]];
        if off == 0 {
            let mut h = format!("// 0x{:x} <{}>", entry.info.addr, entry.info.name);
            if let Some(sec) = &entry.info.section {
                h.push_str(&format!("  {sec}"));
            }
            return h;
        }
        match entry.status {
            Status::Done => entry
                .lines
                .get(off - 1)
                .map(|toks| toks.iter().map(|t| t.text.as_str()).collect())
                .unwrap_or_default(),
            _ => String::new(),
        }
    }

    /// Comment to prepend when a function came from a stale-version cache.
    fn stale_note(&self, f: &FunctionInfo) -> Option<String> {
        f.stale_version
            .as_ref()
            .map(|v| crate::model::stale_comment(v, self.angr_version.as_deref()))
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
            WorkerMessage::Meta {
                file_format,
                angr_version,
            } => {
                self.file_format.get_or_insert(file_format);
                if let Some(v) = angr_version {
                    self.angr_version.get_or_insert(v);
                }
            }
            WorkerMessage::Catalog { functions } => {
                for f in functions {
                    self.entries
                        .entry(f.addr)
                        .or_insert_with(|| Entry::pending(f));
                }
                self.rebuild_order();
            }
            WorkerMessage::Plan { count } => self.total += count,
            WorkerMessage::Function(f) => {
                self.done_count += 1;
                let addr = f.addr;
                let mut lines =
                    highlight::highlight_lines(f.text.as_deref().unwrap_or(""), &f.spans);
                // Prepend a comment when this was served from a stale cache.
                if let Some(note) = self.stale_note(&f) {
                    lines.insert(
                        0,
                        vec![Tok {
                            text: note,
                            kind: TokKind::Comment,
                        }],
                    );
                }
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
                        stale_version: None,
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
    /// `off`. When `cursor`, every span gets a highlight background and the line
    /// is padded with a background spacer out to `width` so the highlight spans
    /// the full panel.
    fn doc_line(&self, i: usize, off: usize, cursor: bool, width: usize) -> Line<'static> {
        let entry = &self.entries[&self.order[i]];
        let bg = |st: Style| if cursor { st.bg(CURSOR_BG) } else { st };

        let mut spans: Vec<Span> = Vec::new();
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
            spans.push(Span::styled(
                h,
                bg(Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD)),
            ));
        } else {
            let body = off - 1;
            match entry.status {
                // For `Done`, missing past the last code line is the blank
                // separator (no spans).
                Status::Done => {
                    if let Some(toks) = entry.lines.get(body) {
                        for t in toks {
                            spans.push(Span::styled(t.text.clone(), bg(style_for(t.kind))));
                        }
                    }
                }
                // Pending/failed bodies are a single line; anything after is blank.
                Status::Pending if body == 0 => spans.push(Span::styled(
                    "    Decompiling…".to_string(),
                    bg(Style::default().fg(Color::Yellow)),
                )),
                Status::Failed if body == 0 => spans.push(Span::styled(
                    format!(
                        "    ✗ {}",
                        entry.error.as_deref().unwrap_or("decompilation failed")
                    ),
                    bg(Style::default().fg(Color::Red)),
                )),
                _ => {}
            }
        }

        // Extend the highlight to the right edge of the panel.
        if cursor {
            let used: usize = spans.iter().map(Span::width).sum();
            if used < width {
                spans.push(Span::styled(
                    " ".repeat(width - used),
                    Style::default().bg(CURSOR_BG),
                ));
            }
        }
        Line::from(spans)
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
        app.apply(WorkerMessage::Meta {
            file_format: fmt,
            angr_version: result.angr_version,
        });
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
                // Ctrl-C always quits.
                if matches!(
                    (key.code, key.modifiers),
                    (KeyCode::Char('c'), KeyModifiers::CONTROL)
                ) {
                    return Ok(());
                }
                // While the "go to" dialog is open, keys feed the text input.
                if app.goto.is_some() {
                    match key.code {
                        KeyCode::Esc => app.goto = None,
                        KeyCode::Enter => app.goto_submit(),
                        KeyCode::Backspace => app.goto_backspace(),
                        KeyCode::Char(c) => app.goto_push(c),
                        _ => {}
                    }
                    continue;
                }
                // Likewise for the search dialog.
                if app.search.is_some() {
                    match key.code {
                        KeyCode::Esc => app.search = None,
                        KeyCode::Enter => app.search_submit(),
                        KeyCode::Backspace => app.search_backspace(),
                        KeyCode::Char(c) => app.search_push(c),
                        _ => {}
                    }
                    continue;
                }
                match (key.code, key.modifiers) {
                    (KeyCode::Char('q'), _) | (KeyCode::Esc, _) => return Ok(()),
                    (KeyCode::Char('g'), _) => app.open_goto(),
                    (KeyCode::Char('/'), _) => app.open_search(),
                    (KeyCode::F(3), _) => app.search_next(true),
                    (KeyCode::F(4), _) => app.search_next(false),
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
        Paragraph::new(
            " j/k line   ./, func   g goto   / search   F3/F4 next/prev   f panel   h help   q quit",
        )
        .style(Style::default().fg(Color::DarkGray)),
        footer,
    );

    if app.show_help {
        render_help(f);
    }
    if let Some(query) = &app.goto {
        render_goto(f, query, app.goto_failed);
    }
    if let Some(query) = &app.search {
        render_search(f, query, app.search_failed);
    }
}

/// Centered full-text search dialog.
fn render_search(f: &mut Frame, query: &str, failed: bool) {
    let area = centered_rect(56, 6, f.area());
    f.render_widget(Clear, area);
    let (title, color) = if failed {
        ("Search — not found", Color::Red)
    } else {
        ("Search", Color::White)
    };
    let text = format!("\n  /{query}▏\n\n  Enter: find · F3: next · F4: prev · Esc: cancel");
    f.render_widget(
        Paragraph::new(text).block(
            Block::new()
                .borders(Borders::ALL)
                .title(title)
                .style(Style::default().fg(color)),
        ),
        area,
    );
}

/// Centered "go to function" input dialog.
fn render_goto(f: &mut Frame, query: &str, failed: bool) {
    let area = centered_rect(56, 6, f.area());
    f.render_widget(Clear, area);
    let (title, color) = if failed {
        ("Go to — not found", Color::Red)
    } else {
        ("Go to function", Color::White)
    };
    let text = format!("\n  {query}▏\n\n  name or 0xADDR · Enter: go · Esc: cancel");
    f.render_widget(
        Paragraph::new(text).block(
            Block::new()
                .borders(Borders::ALL)
                .title(title)
                .style(Style::default().fg(color)),
        ),
        area,
    );
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
        let msg = app
            .fatal
            .clone()
            .unwrap_or_else(|| app.build_status.clone());
        f.render_widget(Paragraph::new(format!("\n  {msg}")).block(block), area);
        return;
    }

    let view_h = area.height.saturating_sub(2) as usize;
    let inner_w = area.width.saturating_sub(2) as usize;
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
        lines.push(app.doc_line(i, off, abs == cursor, inner_w));
    }

    f.render_widget(Paragraph::new(lines).block(block), area);
}

fn render_help(f: &mut Frame) {
    let area = centered_rect(64, 14, f.area());
    let text = "\n  j / k         move the highlighted line (crosses functions)\n  PgUp / PgDn   scroll by a page\n  . / ,         jump to next / previous function\n  g             go to a function by name or address\n  /             full-text search\n  F3 / F4       next / previous search match\n  f             toggle the function panel\n  t             toggle decompilation times\n  h             toggle this help\n  q / Esc       quit";
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

/// Resolve a "go to" query against `funcs` (`(addr, name, size)`, sorted by
/// address) to an index. Matches, in order: an exact function name, an exact
/// address, then an address falling within a function's range. Bare numbers and
/// `0x…` are both read as hex (that's how addresses are shown).
fn resolve_goto(query: &str, funcs: &[(u64, String, u64)]) -> Option<usize> {
    let q = query.trim();
    if q.is_empty() {
        return None;
    }
    if let Some(i) = funcs.iter().position(|(_, name, _)| name == q) {
        return Some(i);
    }
    let addr = parse_addr(q)?;
    if let Some(i) = funcs.iter().position(|(a, _, _)| *a == addr) {
        return Some(i);
    }
    funcs
        .iter()
        .position(|(a, _, size)| *a <= addr && addr < a.saturating_add((*size).max(1)))
}

/// Find the first document line (of `n`) matching `needle` (already
/// lowercased), scanning from `start` in the given direction and wrapping
/// around. `text(abs)` yields a line's plain text. Returns its index, or `None`.
fn find_match(
    n: usize,
    needle: &str,
    start: usize,
    forward: bool,
    text: impl Fn(usize) -> String,
) -> Option<usize> {
    if n == 0 || needle.is_empty() {
        return None;
    }
    for k in 0..n {
        let abs = if forward {
            (start + k) % n
        } else {
            ((start as i64 - k as i64).rem_euclid(n as i64)) as usize
        };
        if text(abs).to_lowercase().contains(needle) {
            return Some(abs);
        }
    }
    None
}

/// Parse an address: optional `0x`/`0X` prefix, hex digits.
fn parse_addr(s: &str) -> Option<u64> {
    let s = s
        .strip_prefix("0x")
        .or_else(|| s.strip_prefix("0X"))
        .unwrap_or(s);
    u64::from_str_radix(s, 16).ok()
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

#[cfg(test)]
mod tests {
    use super::*;

    fn funcs() -> Vec<(u64, String, u64)> {
        vec![
            (0x1000, "main".to_string(), 0x20),
            (0x2000, "sub_2000".to_string(), 0x10),
        ]
    }

    #[test]
    fn goto_by_name() {
        let f = funcs();
        assert_eq!(resolve_goto("main", &f), Some(0));
        assert_eq!(resolve_goto("sub_2000", &f), Some(1));
        assert_eq!(resolve_goto("  main  ", &f), Some(0)); // trimmed
    }

    #[test]
    fn goto_by_address() {
        let f = funcs();
        assert_eq!(resolve_goto("0x2000", &f), Some(1));
        assert_eq!(resolve_goto("2000", &f), Some(1)); // bare hex
        assert_eq!(resolve_goto("0X1000", &f), Some(0));
    }

    #[test]
    fn goto_address_inside_function() {
        let f = funcs();
        assert_eq!(resolve_goto("0x1010", &f), Some(0)); // within main [0x1000,0x1020)
        assert_eq!(resolve_goto("0x2005", &f), Some(1)); // within sub_2000
    }

    #[test]
    fn goto_no_match() {
        let f = funcs();
        assert_eq!(resolve_goto("", &f), None);
        assert_eq!(resolve_goto("nope", &f), None);
        assert_eq!(resolve_goto("0x9999", &f), None); // outside every function
    }

    #[test]
    fn search_forward_backward_and_wrap() {
        let lines = [
            "aaa".to_string(),
            "the MATCH here".to_string(),
            "bbb".to_string(),
            "another match".to_string(),
        ];
        let n = lines.len();
        let text = |i: usize| lines[i].clone();
        // case-insensitive, forward from 0 -> line 1
        assert_eq!(find_match(n, "match", 0, true, text), Some(1));
        // forward from line 2 -> line 3
        assert_eq!(find_match(n, "match", 2, true, text), Some(3));
        // forward from line 3+1 wraps past end back to line 1
        assert_eq!(find_match(n, "match", (3 + 1) % n, true, text), Some(1));
        // backward from line 0 wraps to line 3
        assert_eq!(find_match(n, "match", 0, false, text), Some(3));
        // no match / empty needle
        assert_eq!(find_match(n, "zzz", 0, true, text), None);
        assert_eq!(find_match(n, "", 0, true, text), None);
        assert_eq!(find_match(0, "x", 0, true, |_| String::new()), None);
    }
}
