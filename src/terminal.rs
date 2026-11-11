//! A PTY running Claude Code, parsed by alacritty_terminal and drawn on a
//! monospace grid with GPUI.

use std::{
    cell::Cell,
    io::{Read, Write},
    rc::Rc,
    sync::mpsc,
};

use alacritty_terminal::{
    event::{Event as TermEvent, EventListener, WindowSize},
    grid::{Dimensions, Scroll},
    index::{Column, Line, Point as GridPoint, Side},
    selection::{Selection, SelectionType},
    term::{Config, Term, TermMode, cell::Flags, test::TermSize},
    vte::ansi::{Color as AnsiColor, CursorShape, NamedColor, Processor, Rgb},
};
use gpui::{
    App, Bounds, ClipboardItem, Context, EventEmitter, FocusHandle, Focusable, Font, FontStyle,
    FontWeight, Hsla, KeyDownEvent, Keystroke, MouseButton, MouseDownEvent, MouseMoveEvent,
    MouseUpEvent, Pixels, Point, Rgba, ScrollDelta, ScrollWheelEvent, SharedString,
    StrikethroughStyle, Task, TextRun, UnderlineStyle, Window, canvas, div, fill, font, point,
    prelude::*, px, size,
};
use anyhow::Context as _;
use portable_pty::{Child, CommandBuilder, MasterPty, PtySize, native_pty_system};

use crate::{Copy, Paste, accounts::Account, glass, guard, git_identity::{self, GitIdentity}, settings::ThemeSetting, theme::Theme, usage, worktree::Workspace};

/// Pty reads (up to 64 KB each) waiting to be parsed: at most ~4 MB.
const OUTPUT_QUEUE_CHUNKS: usize = 64;
/// Most output parsed in one update before yielding to the UI.
const MAX_INGEST_BYTES: usize = 1024 * 1024;

/// `first` plus whatever else is already queued, up to MAX_INGEST_BYTES
/// (one read can take it a little over). True when it stopped at the limit.
fn next_batch(mut bytes: Vec<u8>, rx: &smol::channel::Receiver<Vec<u8>>) -> (Vec<u8>, bool) {
    while bytes.len() < MAX_INGEST_BYTES {
        let Ok(more) = rx.try_recv() else { break };
        bytes.extend(more);
    }
    let full = bytes.len() >= MAX_INGEST_BYTES;
    (bytes, full)
}

/// Reads the pty in 64 KB pieces into the bounded output queue until EOF.
fn spawn_pty_reader(mut reader: Box<dyn Read + Send>, tx: smol::channel::Sender<Vec<u8>>) {
    std::thread::spawn(move || {
        let mut buf = vec![0u8; 64 * 1024];
        loop {
            match reader.read(&mut buf) {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    if tx.send_blocking(buf[..n].to_vec()).is_err() {
                        break;
                    }
                }
            }
        }
    });
}

#[derive(Clone)]
struct Listener(mpsc::Sender<TermEvent>);

impl EventListener for Listener {
    fn send_event(&self, event: TermEvent) {
        let _ = self.0.send(event);
    }
}

struct Pty {
    master: Box<dyn MasterPty + Send>,
    writer: Box<dyn Write + Send>,
    child: Box<dyn Child + Send + Sync>,
}

pub enum SessionEvent {
    TitleChanged,
    Exited,
    Focused,
}

pub struct TerminalSession {
    pub id: usize,
    pub workspace: Workspace,
    /// Branch checked out in the session's directory (looked up once).
    pub branch: Option<String>,
    /// Claude account (profile) this session runs as.
    pub account: Account,
    /// GitHub identity (commit author, SSH key, gh login) for this window.
    pub git: Option<GitIdentity>,
    /// Conversation to resume on the next start.
    resume: Option<String>,
    /// This window's own theme; `None` follows the app's default theme.
    pub theme_override: Option<(ThemeSetting, Theme)>,
    pub title: Option<SharedString>,
    pub exit_status: Option<String>,
    command: String,
    font_family: SharedString,
    font_size: Pixels,
    term: Term<Listener>,
    events: mpsc::Receiver<TermEvent>,
    parser: Processor,
    pty: Option<Pty>,
    focus_handle: FocusHandle,
    cols: usize,
    rows: usize,
    cell: gpui::Size<Pixels>,
    grid_bounds: Rc<Cell<Option<Bounds<Pixels>>>>,
    scroll_remainder: f32,
    selecting: bool,
    /// Link under the mouse: its URL and the grid cells it covers.
    hovered_link: Option<Link>,
    /// Where the current left-button press started, if it may become a link click.
    press_point: Option<GridPoint>,
    _io: Option<Task<()>>,
}

impl EventEmitter<SessionEvent> for TerminalSession {}

impl Focusable for TerminalSession {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl TerminalSession {
    pub fn new(
        id: usize,
        workspace: Workspace,
        branch: Option<String>,
        account: Account,
        git: Option<GitIdentity>,
        command: String,
        font_family: String,
        font_size: f32,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let (tx, events) = mpsc::channel();
        let (cols, rows) = (100, 30);
        let mut this = Self {
            id,
            branch,
            workspace,
            title: None,
            exit_status: None,
            account,
            git,
            resume: None,
            theme_override: None,
            command,
            font_family: font_family.into(),
            font_size: px(font_size),
            term: Term::new(Config::default(), &TermSize::new(cols, rows), Listener(tx)),
            events,
            parser: Processor::new(),
            pty: None,
            focus_handle: cx.focus_handle(),
            cols,
            rows,
            cell: size(px(8.), px(17.)),
            grid_bounds: Rc::new(Cell::new(None)),
            scroll_remainder: 0.,
            selecting: false,
            hovered_link: None,
            press_point: None,
            _io: None,
        };
        this.start(cx);
        cx.on_focus(&this.focus_handle, window, |this, _, cx| {
            this.report_focus(true);
            cx.emit(SessionEvent::Focused);
        })
        .detach();
        cx.on_blur(&this.focus_handle, window, |this, _, _| this.report_focus(false)).detach();
        this
    }

    /// The theme this window is drawn with.
    pub fn theme(&self, cx: &App) -> Theme {
        let theme = match &self.theme_override {
            Some((_, theme)) => theme.clone(),
            None => cx.global::<Theme>().clone(),
        };
        if glass::enabled(cx) { theme.glassy() } else { theme }
    }

    pub fn label(&self) -> SharedString {
        self.title.clone().unwrap_or_else(|| format!("Claude {}", self.id).into())
    }

    pub fn is_running(&self) -> bool {
        self.exit_status.is_none()
    }

    /// Restart Claude with this account and GitHub identity, optionally
    /// resuming a conversation.
    pub fn restart_with(&mut self, account: Account, git: Option<GitIdentity>, resume: Option<String>, cx: &mut Context<Self>) {
        self.account = account;
        self.git = git;
        self.resume = resume;
        self.restart(cx);
    }

    pub fn restart(&mut self, cx: &mut Context<Self>) {
        self.kill();
        let (tx, events) = mpsc::channel();
        self.term = Term::new(Config::default(), &TermSize::new(self.cols, self.rows), Listener(tx));
        self.events = events;
        self.parser = Processor::new();
        self.title = None;
        self.start(cx);
        cx.emit(SessionEvent::TitleChanged);
        cx.notify();
    }

    fn start(&mut self, cx: &mut Context<Self>) {
        match self.spawn_pty(cx) {
            Ok(()) => self.exit_status = None,
            Err(e) => self.exit_status = Some(format!("Failed to start: {e:#}")),
        }
    }

    fn spawn_pty(&mut self, cx: &mut Context<Self>) -> anyhow::Result<()> {
        let pair = native_pty_system().openpty(self.pty_size())?;
        // A login + interactive shell picks up the user's PATH even when the
        // app was launched from Finder.
        let shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/zsh".into());
        let mut cmd = CommandBuilder::new(&shell);
        let resume = self.resume.take().map(|id| format!(" --resume {id}")).unwrap_or_default();
        // Safety gates (see guard.rs; only for sessions on Claude Deck's own
        // repo), plus our status line, which records this account's plan usage
        // for the usage bar. If the file can't be written, no session.
        let status_line = usage::status_line(&self.account.id).map(|v| ("statusLine", v));
        let settings = guard::session_settings(self.id, &self.workspace, status_line)
            .context("couldn't write the session's safety settings")?;
        let settings = settings.to_string_lossy().replace('\'', r"'\''");
        cmd.args(["-l", "-i", "-c", &format!("exec {}{resume} --settings '{settings}'", self.command)]);
        cmd.cwd(&self.workspace.cwd);
        cmd.env("TERM", "xterm-256color");
        cmd.env("COLORTERM", "truecolor");
        cmd.env("CLAUDE_DECK_SESSION", self.id.to_string());
        self.account.apply_env(&mut cmd);
        match &self.git {
            Some(git) => git.apply_env(&mut cmd),
            None => git_identity::clear_env(&mut cmd),
        }
        if std::env::var("LANG").is_err() {
            cmd.env("LANG", "en_US.UTF-8");
        }
        let child = pair.slave.spawn_command(cmd)?;
        drop(pair.slave);

        let reader = pair.master.try_clone_reader()?;
        let writer = pair.master.take_writer()?;
        // Bounded, so a flood of output (a big `cat`) waits in the pty, as in
        // any terminal, instead of piling up in memory faster than we parse.
        let (tx, rx) = smol::channel::bounded::<Vec<u8>>(OUTPUT_QUEUE_CHUNKS);
        spawn_pty_reader(reader, tx);
        self._io = Some(cx.spawn(async move |this, cx| {
            while let Ok(first) = rx.recv().await {
                // Parsing runs on the UI thread: after a full batch, yield so
                // a frame and key presses get through before the next one.
                let (bytes, full) = next_batch(first, &rx);
                if this.update(cx, |t, cx| t.ingest(&bytes, cx)).is_err() {
                    return;
                }
                if full {
                    smol::future::yield_now().await;
                }
            }
            let _ = this.update(cx, |t, cx| t.on_exit(cx));
        }));
        self.pty = Some(Pty { master: pair.master, writer, child });
        Ok(())
    }

    pub fn kill(&mut self) {
        if let Some(mut pty) = self.pty.take() {
            let _ = pty.child.kill();
            let _ = pty.child.wait();
        }
        self._io = None;
    }

    fn on_exit(&mut self, cx: &mut Context<Self>) {
        let status = self.pty.as_mut().and_then(|p| p.child.wait().ok());
        self.pty = None;
        self.exit_status = Some(match status {
            Some(s) if s.success() => "Claude exited".into(),
            Some(s) => format!("Claude exited with code {}", s.exit_code()),
            None => "Claude exited".into(),
        });
        cx.emit(SessionEvent::Exited);
        cx.notify();
    }

    fn ingest(&mut self, bytes: &[u8], cx: &mut Context<Self>) {
        self.parser.advance(&mut self.term, bytes);
        while let Ok(event) = self.events.try_recv() {
            match event {
                TermEvent::PtyWrite(s) => self.write(s.as_bytes()),
                TermEvent::Title(t) => {
                    self.title = Some(t.into());
                    cx.emit(SessionEvent::TitleChanged);
                }
                TermEvent::ResetTitle => {
                    self.title = None;
                    cx.emit(SessionEvent::TitleChanged);
                }
                TermEvent::ClipboardStore(_, text) => cx.write_to_clipboard(ClipboardItem::new_string(text)),
                TermEvent::ColorRequest(index, format) => {
                    let color = self.query_color(index, cx);
                    self.write(format(color).as_bytes());
                }
                TermEvent::TextAreaSizeRequest(format) => {
                    let reply = format(self.window_size());
                    self.write(reply.as_bytes());
                }
                _ => {}
            }
        }
        cx.notify();
    }

    fn write(&mut self, bytes: &[u8]) {
        if let Some(pty) = &mut self.pty {
            let _ = pty.writer.write_all(bytes);
            let _ = pty.writer.flush();
        }
    }

    fn report_focus(&mut self, focused: bool) {
        if self.term.mode().contains(TermMode::FOCUS_IN_OUT) {
            self.write(if focused { b"\x1b[I" } else { b"\x1b[O" });
        }
    }

    fn window_size(&self) -> WindowSize {
        WindowSize {
            num_lines: self.rows as u16,
            num_cols: self.cols as u16,
            cell_width: f32::from(self.cell.width) as u16,
            cell_height: f32::from(self.cell.height) as u16,
        }
    }

    fn pty_size(&self) -> PtySize {
        PtySize {
            rows: self.rows as u16,
            cols: self.cols as u16,
            pixel_width: (self.cols as f32 * f32::from(self.cell.width)) as u16,
            pixel_height: (self.rows as f32 * f32::from(self.cell.height)) as u16,
        }
    }

    fn resize(&mut self, cols: usize, rows: usize) {
        if (cols, rows) == (self.cols, self.rows) {
            return;
        }
        self.cols = cols;
        self.rows = rows;
        self.term.resize(TermSize::new(cols, rows));
        if let Some(pty) = &self.pty {
            let _ = pty.master.resize(self.pty_size());
        }
    }

    fn query_color(&self, index: usize, cx: &App) -> Rgb {
        let theme = self.theme(cx);
        let color = self.term.colors()[index].map(rgb_to_hsla).unwrap_or(match index {
            0..=15 => theme.ansi[index],
            256 => theme.term_fg,
            257 => theme.term_bg,
            _ => theme.term_cursor,
        });
        let rgba = color.to_rgb();
        Rgb { r: (rgba.r * 255.) as u8, g: (rgba.g * 255.) as u8, b: (rgba.b * 255.) as u8 }
    }

    // ---- input ---------------------------------------------------------

    /// Type text into the session as if from the keyboard.
    pub fn type_text(&mut self, text: &str) {
        self.send_input(text.as_bytes());
    }

    /// The visible screen as plain text.
    pub fn screen_text(&self) -> String {
        let content = self.term.renderable_content();
        let offset = content.display_offset as i32;
        let mut lines = vec![String::new(); self.rows];
        for indexed in content.display_iter {
            let row = indexed.point.line.0 + offset;
            if row >= 0 && (row as usize) < self.rows {
                lines[row as usize].push(indexed.cell.c);
            }
        }
        lines.iter().map(|l| l.trim_end()).collect::<Vec<_>>().join("\n")
    }

    fn send_input(&mut self, bytes: &[u8]) {
        self.term.selection = None;
        self.term.scroll_display(Scroll::Bottom);
        self.write(bytes);
    }

    /// Process id of the command running in this session's terminal.
    pub fn pid(&self) -> Option<u32> {
        self.pty.as_ref()?.child.process_id()
    }

    fn key_down(&mut self, event: &KeyDownEvent, _: &mut Window, cx: &mut Context<Self>) {
        let app_cursor = self.term.mode().contains(TermMode::APP_CURSOR);
        if let Some(bytes) = key_to_bytes(&event.keystroke, app_cursor) {
            self.send_input(&bytes);
            cx.stop_propagation();
            cx.notify();
        }
    }

    fn paste(&mut self, _: &Paste, _: &mut Window, cx: &mut Context<Self>) {
        let Some(text) = cx.read_from_clipboard().and_then(|item| item.text()) else { return };
        let text = text.replace("\r\n", "\r").replace('\n', "\r");
        if self.term.mode().contains(TermMode::BRACKETED_PASTE) {
            let clean = text.replace('\x1b', "");
            self.send_input(format!("\x1b[200~{clean}\x1b[201~").as_bytes());
        } else {
            self.send_input(text.as_bytes());
        }
        cx.notify();
    }

    fn copy(&mut self, _: &Copy, _: &mut Window, cx: &mut Context<Self>) {
        if let Some(text) = self.term.selection_to_string() {
            cx.write_to_clipboard(ClipboardItem::new_string(text));
        }
    }

    fn scroll(&mut self, event: &ScrollWheelEvent, _: &mut Window, cx: &mut Context<Self>) {
        let lines = match event.delta {
            ScrollDelta::Lines(p) => p.y,
            ScrollDelta::Pixels(p) => f32::from(p.y) / f32::from(self.cell.height),
        } + self.scroll_remainder;
        let whole = lines.trunc();
        self.scroll_remainder = lines - whole;
        let whole = whole as i32;
        if whole == 0 {
            return;
        }
        let mode = *self.term.mode();
        if mode.intersects(TermMode::MOUSE_MODE) {
            // The app tracks the mouse (e.g. Claude Code's fullscreen TUI):
            // report wheel events so it scrolls its own transcript.
            let (col, row) = self.cell_at(event.position);
            let m = &event.modifiers;
            let button = if whole > 0 { 64 } else { 65 } + 4 * (m.shift as u8) + 8 * (m.alt as u8) + 16 * (m.control as u8);
            let seq = mouse_report(button, col, row, mode);
            for _ in 0..whole.abs() {
                self.write(&seq);
            }
        } else if mode.contains(TermMode::ALT_SCREEN) && mode.contains(TermMode::ALTERNATE_SCROLL) {
            let seq: &[u8] = match (whole > 0, mode.contains(TermMode::APP_CURSOR)) {
                (true, true) => b"\x1bOA",
                (true, false) => b"\x1b[A",
                (false, true) => b"\x1bOB",
                (false, false) => b"\x1b[B",
            };
            for _ in 0..whole.abs() {
                self.write(seq);
            }
        } else {
            self.term.scroll_display(Scroll::Delta(whole));
        }
        cx.notify();
    }

    /// Zero-based viewport (column, row) under `position`.
    fn cell_at(&self, position: Point<Pixels>) -> (usize, usize) {
        let Some(bounds) = self.grid_bounds.get() else { return (0, 0) };
        let x = f32::from(position.x - bounds.origin.x).max(0.);
        let y = f32::from(position.y - bounds.origin.y).max(0.);
        let col = ((x / f32::from(self.cell.width)) as usize).min(self.cols.saturating_sub(1));
        let row = ((y / f32::from(self.cell.height)) as usize).min(self.rows.saturating_sub(1));
        (col, row)
    }

    fn grid_point(&self, position: Point<Pixels>) -> Option<(GridPoint, Side)> {
        let bounds = self.grid_bounds.get()?;
        let x = f32::from(position.x - bounds.origin.x).max(0.);
        let y = f32::from(position.y - bounds.origin.y).max(0.);
        let col_f = x / f32::from(self.cell.width);
        let col = (col_f as usize).min(self.cols.saturating_sub(1));
        let row = ((y / f32::from(self.cell.height)) as usize).min(self.rows.saturating_sub(1));
        let side = if col_f.fract() < 0.5 { Side::Left } else { Side::Right };
        let line = Line(row as i32 - self.term.grid().display_offset() as i32);
        Some((GridPoint::new(line, Column(col)), side))
    }

    /// The link (OSC 8 hyperlink or plain-text URL) covering `point`, if any.
    fn link_at(&self, point: GridPoint) -> Option<Link> {
        let grid = self.term.grid();
        let last_col = grid.columns() - 1;
        let wraps = |line: Line| grid[line][Column(last_col)].flags.contains(Flags::WRAPLINE);

        // The logical line around `point`, following soft wraps both ways.
        let mut start = point.line;
        while start > grid.topmost_line() && wraps(start - 1) {
            start -= 1;
        }
        let mut end = point.line;
        while end < grid.bottommost_line() && wraps(end) {
            end += 1;
        }
        let mut cells = Vec::new();
        let mut text = String::new();
        for line in start.0..=end.0 {
            for col in 0..=last_col {
                let p = GridPoint::new(Line(line), Column(col));
                let cell = &grid[p];
                if cell.flags.intersects(Flags::WIDE_CHAR_SPACER | Flags::LEADING_WIDE_CHAR_SPACER) {
                    continue;
                }
                cells.push((p, cell.hyperlink().map(|h| h.uri().to_string())));
                text.push(if cell.c == '\0' { ' ' } else { cell.c });
            }
        }
        let index = cells.iter().position(|(p, _)| *p >= point)?;

        // OSC 8 hyperlink: the run of cells sharing the clicked cell's URI.
        if let Some(uri) = cells[index].1.clone() {
            let same = |i: &usize| cells[*i].1.as_deref() == Some(uri.as_str());
            let first = (0..=index).rev().take_while(same).last()?;
            let last = (index..cells.len()).take_while(same).last()?;
            return Some(Link { url: uri, start: cells[first].0, end: cells[last].0 });
        }

        let chars: Vec<char> = text.chars().collect();
        let (first, last) = url_span(&chars, index)?;
        Some(Link { url: chars[first..=last].iter().collect(), start: cells[first].0, end: cells[last].0 })
    }

    fn mouse_down(&mut self, event: &MouseDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        window.focus(&self.focus_handle);
        let Some((point, side)) = self.grid_point(event.position) else { return };
        self.press_point = (event.click_count == 1 && !event.modifiers.shift).then_some(point);
        let kind = match event.click_count {
            2 => SelectionType::Semantic,
            3 => SelectionType::Lines,
            _ => SelectionType::Simple,
        };
        match &mut self.term.selection {
            Some(sel) if event.modifiers.shift => sel.update(point, side),
            _ => self.term.selection = Some(Selection::new(kind, point, side)),
        }
        self.selecting = true;
        cx.notify();
    }

    fn mouse_move(&mut self, event: &MouseMoveEvent, _: &mut Window, cx: &mut Context<Self>) {
        if event.pressed_button.is_none() {
            let link = self.grid_point(event.position).and_then(|(point, _)| self.link_at(point));
            if link != self.hovered_link {
                self.hovered_link = link;
                cx.notify();
            }
        }
        if !self.selecting || event.pressed_button != Some(MouseButton::Left) {
            return;
        }
        if let Some((point, side)) = self.grid_point(event.position) {
            if let Some(sel) = &mut self.term.selection {
                sel.update(point, side);
                cx.notify();
            }
        }
    }

    fn mouse_up(&mut self, event: &MouseUpEvent, _: &mut Window, cx: &mut Context<Self>) {
        self.selecting = false;
        // A click that didn't drag onto another cell opens the link under it.
        if let (Some(down), Some((up, _))) = (self.press_point.take(), self.grid_point(event.position)) {
            if down == up {
                if let Some(link) = self.link_at(up) {
                    cx.open_url(&link.url);
                    self.term.selection = None;
                    cx.notify();
                    return;
                }
            }
        }
        if self.term.selection.as_ref().is_some_and(|s| s.is_empty()) {
            self.term.selection = None;
            cx.notify();
        }
    }

    // ---- rendering -------------------------------------------------------

    fn snapshot(&self, theme: &Theme, focused: bool) -> Vec<RenderLine> {
        let content = self.term.renderable_content();
        let offset = content.display_offset as i32;
        let selection = content.selection;
        let cursor = content.cursor;
        let show_cursor = cursor.shape != CursorShape::Hidden;
        let colors = content.colors;
        let link = self.hovered_link.as_ref();

        let resolve = |color: AnsiColor| -> Option<Hsla> {
            let (index, fallback) = match color {
                AnsiColor::Spec(rgb) => return Some(rgb_to_hsla(rgb)),
                AnsiColor::Indexed(i) => (i as usize, indexed_color(i, theme)),
                AnsiColor::Named(n) => {
                    let i = n as usize;
                    let fallback = match n {
                        NamedColor::Foreground | NamedColor::BrightForeground => theme.term_fg,
                        NamedColor::Background => return colors[i].map(rgb_to_hsla),
                        NamedColor::Cursor => theme.term_cursor,
                        NamedColor::DimForeground => Hsla { a: 0.66, ..theme.term_fg },
                        _ if i < 16 => theme.ansi[i],
                        // Dim variants: DimBlack..DimWhite map onto 0..7
                        _ => {
                            let base = theme.ansi[(i - NamedColor::DimBlack as usize).min(7)];
                            Hsla { a: 0.66, ..base }
                        }
                    };
                    (i, fallback)
                }
            };
            Some(colors[index].map(rgb_to_hsla).unwrap_or(fallback))
        };

        let mut cells: Vec<Vec<CellStyle>> = vec![Vec::with_capacity(self.cols); self.rows];
        for indexed in content.display_iter {
            let row = indexed.point.line.0 + offset;
            if row < 0 || row as usize >= self.rows {
                continue;
            }
            let cell = indexed.cell;
            let flags = cell.flags;
            let mut fg = resolve(cell.fg).unwrap_or(theme.term_fg);
            let mut bg = resolve(cell.bg);
            if flags.contains(Flags::INVERSE) {
                let old_fg = fg;
                fg = bg.unwrap_or(theme.term_bg);
                bg = Some(old_fg);
            }
            if flags.contains(Flags::DIM) {
                fg.a *= 0.66;
            }
            // Over glass the backdrop varies, so faint text needs a floor.
            if theme.glass && bg.is_none() {
                fg = theme.legible(fg);
            }
            if flags.contains(Flags::HIDDEN) {
                fg = bg.unwrap_or(theme.term_bg);
            }
            if selection.is_some_and(|s| s.contains(indexed.point)) {
                bg = Some(theme.term_selection);
            }
            let is_cursor = show_cursor && offset == 0 && indexed.point == cursor.point;
            let mut cursor_mark = None;
            if is_cursor {
                if focused && cursor.shape == CursorShape::Block {
                    bg = Some(theme.term_cursor);
                    fg = theme.term_bg;
                } else {
                    cursor_mark = Some(if focused { cursor.shape } else { CursorShape::HollowBlock });
                }
            }
            let ch = if flags.intersects(Flags::WIDE_CHAR_SPACER | Flags::LEADING_WIDE_CHAR_SPACER) || cell.c == '\0' || cell.c == '\t' {
                ' '
            } else {
                cell.c
            };
            cells[row as usize].push(CellStyle {
                ch,
                fg,
                bg,
                bold: flags.contains(Flags::BOLD),
                italic: flags.contains(Flags::ITALIC),
                underline: flags.intersects(Flags::ALL_UNDERLINES)
                    || link.is_some_and(|l| (l.start..=l.end).contains(&indexed.point)),
                strike: flags.contains(Flags::STRIKEOUT),
                cursor: cursor_mark,
            });
        }

        let base_font = font(self.font_family.clone());
        cells
            .into_iter()
            .map(|row| {
                let mut line = RenderLine::default();
                for (col, cell) in row.iter().enumerate() {
                    let start = line.text.len();
                    line.text.push(cell.ch);
                    let len = line.text.len() - start;
                    match line.runs.last_mut() {
                        Some((style, run)) if style.same_text_style(cell) => run.len += len,
                        _ => {
                            let mut f = base_font.clone();
                            if cell.bold {
                                f.weight = FontWeight::BOLD;
                            }
                            if cell.italic {
                                f.style = FontStyle::Italic;
                            }
                            line.runs.push((cell.clone(), text_run(len, f, cell)));
                        }
                    }
                    if let Some(bg) = cell.bg {
                        match line.backgrounds.last_mut() {
                            Some((s, n, c)) if *s + *n == col && *c == bg => *n += 1,
                            _ => line.backgrounds.push((col, 1, bg)),
                        }
                    }
                    if let Some(shape) = cell.cursor {
                        line.cursor = Some((col, shape));
                    }
                }
                line
            })
            .collect()
    }
}

#[derive(Clone, PartialEq)]
struct Link {
    url: String,
    start: GridPoint,
    end: GridPoint,
}

/// The `http(s)://` URL in `chars` covering index `at`, as an inclusive range.
fn url_span(chars: &[char], at: usize) -> Option<(usize, usize)> {
    let is_url_char = |c: char| c.is_ascii_graphic() && !matches!(c, '<' | '>' | '"' | '`' | '{' | '}' | '|' | '\\' | '^');
    let mut start = at;
    while start > 0 && is_url_char(chars[start - 1]) {
        start -= 1;
    }
    let mut end = at;
    while end + 1 < chars.len() && is_url_char(chars[end + 1]) {
        end += 1;
    }
    if !is_url_char(*chars.get(at)?) {
        return None;
    }
    // The URL starts at the scheme; anything glued on before it (quotes, brackets) isn't part of it.
    let word: String = chars[start..=end].iter().collect();
    let offset = ["https://", "http://"].iter().filter_map(|s| word.find(s)).min()?;
    let start = start + word[..offset].chars().count();
    if at < start {
        return None;
    }
    // Drop trailing punctuation and unbalanced closing brackets.
    loop {
        let c = chars[end];
        let unbalanced = |open: char| {
            let s = &chars[start..=end];
            s.iter().filter(|&&x| x == c).count() > s.iter().filter(|&&x| x == open).count()
        };
        let trim = matches!(c, '.' | ',' | ';' | ':' | '!' | '?' | '\'')
            || (c == ')' && unbalanced('('))
            || (c == ']' && unbalanced('['));
        if !trim || end <= start {
            break;
        }
        end -= 1;
    }
    let scheme_len = if chars[start + 4] == 's' { 8 } else { 7 };
    (end >= start + scheme_len && at <= end).then_some((start, end))
}

#[derive(Clone)]
struct CellStyle {
    ch: char,
    fg: Hsla,
    bg: Option<Hsla>,
    bold: bool,
    italic: bool,
    underline: bool,
    strike: bool,
    cursor: Option<CursorShape>,
}

impl CellStyle {
    fn same_text_style(&self, other: &CellStyle) -> bool {
        self.fg == other.fg
            && self.bold == other.bold
            && self.italic == other.italic
            && self.underline == other.underline
            && self.strike == other.strike
    }
}

fn text_run(len: usize, font: Font, cell: &CellStyle) -> TextRun {
    TextRun {
        len,
        font,
        color: cell.fg,
        background_color: None,
        underline: cell.underline.then(|| UnderlineStyle { thickness: px(1.), color: Some(cell.fg), wavy: false }),
        strikethrough: cell.strike.then(|| StrikethroughStyle { thickness: px(1.), color: Some(cell.fg) }),
    }
}

#[derive(Default)]
struct RenderLine {
    text: String,
    runs: Vec<(CellStyle, TextRun)>,
    backgrounds: Vec<(usize, usize, Hsla)>,
    cursor: Option<(usize, CursorShape)>,
}

impl Render for TerminalSession {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = self.theme(cx);
        let focused = self.focus_handle.is_focused(window);

        // Cell metrics for the configured monospace font.
        let text_system = window.text_system();
        let font_id = text_system.resolve_font(&font(self.font_family.clone()));
        let cell_width = text_system
            .advance(font_id, self.font_size, 'm')
            .map(|s| s.width)
            .unwrap_or(px(f32::from(self.font_size) * 0.6));
        let line_height = px((f32::from(self.font_size) * 1.35).round());
        self.cell = size(cell_width, line_height);

        let lines = self.snapshot(&theme, focused);
        let font_size = self.font_size;
        let cursor_color = theme.term_cursor;
        let grid_bounds = self.grid_bounds.clone();
        let (cols, rows) = (self.cols, self.rows);
        let entity = cx.entity();

        let grid = canvas(
            move |bounds, window, cx| {
                grid_bounds.set(Some(bounds));
                let new_cols = ((bounds.size.width / cell_width).floor() as usize).max(20);
                let new_rows = ((bounds.size.height / line_height).floor() as usize).max(4);
                if (new_cols, new_rows) != (cols, rows) {
                    cx.defer(move |cx| {
                        entity.update(cx, |t, cx| {
                            t.resize(new_cols, new_rows);
                            cx.notify();
                        })
                    });
                }
                lines
                    .into_iter()
                    .map(|line| {
                        let runs: Vec<TextRun> = line.runs.into_iter().map(|(_, r)| r).collect();
                        let shaped = window.text_system().shape_line(line.text.into(), font_size, &runs, Some(cell_width));
                        (shaped, line.backgrounds, line.cursor)
                    })
                    .collect::<Vec<_>>()
            },
            move |bounds, lines, window, cx| {
                for (row, (shaped, backgrounds, cursor)) in lines.iter().enumerate() {
                    let y = bounds.origin.y + line_height * row as f32;
                    let cell_origin = |col: usize| point(bounds.origin.x + cell_width * col as f32, y);
                    for (col, len, color) in backgrounds {
                        window.paint_quad(fill(Bounds::new(cell_origin(*col), size(cell_width * *len as f32, line_height)), *color));
                    }
                    if let Some((col, shape)) = cursor {
                        let o = cell_origin(*col);
                        let t = px(1.);
                        let rects = match shape {
                            CursorShape::Beam => vec![Bounds::new(o, size(px(2.), line_height))],
                            CursorShape::Underline => vec![Bounds::new(point(o.x, o.y + line_height - px(2.)), size(cell_width, px(2.)))],
                            _ => vec![
                                Bounds::new(o, size(cell_width, t)),
                                Bounds::new(point(o.x, o.y + line_height - t), size(cell_width, t)),
                                Bounds::new(o, size(t, line_height)),
                                Bounds::new(point(o.x + cell_width - t, o.y), size(t, line_height)),
                            ],
                        };
                        for r in rects {
                            window.paint_quad(fill(r, cursor_color));
                        }
                    }
                    let _ = shaped.paint(point(bounds.origin.x, y), line_height, window, cx);
                }
            },
        )
        .size_full();

        div()
            .id(("terminal", self.id))
            .track_focus(&self.focus_handle)
            .key_context("Terminal")
            .on_action(cx.listener(Self::paste))
            .on_action(cx.listener(Self::copy))
            .on_key_down(cx.listener(Self::key_down))
            .on_scroll_wheel(cx.listener(Self::scroll))
            .on_mouse_down(MouseButton::Left, cx.listener(Self::mouse_down))
            .on_mouse_move(cx.listener(Self::mouse_move))
            .on_mouse_up(MouseButton::Left, cx.listener(Self::mouse_up))
            .when(self.hovered_link.is_some(), |d| d.cursor_pointer())
            .size_full()
            .overflow_hidden()
            .pl(px(10.))
            .pt(px(6.))
            .child(grid)
    }
}

impl Drop for TerminalSession {
    fn drop(&mut self) {
        self.kill();
    }
}

fn rgb_to_hsla(rgb: Rgb) -> Hsla {
    Rgba { r: rgb.r as f32 / 255., g: rgb.g as f32 / 255., b: rgb.b as f32 / 255., a: 1. }.into()
}

fn indexed_color(i: u8, theme: &Theme) -> Hsla {
    match i {
        0..=15 => theme.ansi[i as usize],
        16..=231 => {
            let i = i - 16;
            let level = |v: u8| if v == 0 { 0 } else { v * 40 + 55 };
            rgb_to_hsla(Rgb { r: level(i / 36), g: level((i / 6) % 6), b: level(i % 6) })
        }
        _ => {
            let v = 8 + (i - 232) * 10;
            rgb_to_hsla(Rgb { r: v, g: v, b: v })
        }
    }
}

/// Translate a key press into the bytes a VT terminal would send.
/// Cmd-shortcuts return None so they reach the app's key bindings.
/// Encodes a mouse press of `button` at zero-based (col, row) in the
/// protocol the app asked for: SGR, UTF-8, or classic X10.
fn mouse_report(button: u8, col: usize, row: usize, mode: TermMode) -> Vec<u8> {
    if mode.contains(TermMode::SGR_MOUSE) {
        return format!("\x1b[<{button};{};{}M", col + 1, row + 1).into_bytes();
    }
    let mut seq = b"\x1b[M".to_vec();
    seq.push(32 + button);
    for pos in [col, row] {
        let v = 32 + 1 + pos as u32;
        if mode.contains(TermMode::UTF8_MOUSE) {
            let c = char::from_u32(v.min(2047)).unwrap_or(' ');
            seq.extend(c.to_string().as_bytes());
        } else {
            seq.push(v.min(255) as u8);
        }
    }
    seq
}

fn key_to_bytes(keystroke: &Keystroke, app_cursor: bool) -> Option<Vec<u8>> {
    let m = &keystroke.modifiers;
    if m.platform {
        return None;
    }
    let key = keystroke.key.as_str();
    let modifier_code = 1 + (m.shift as u8) + 2 * (m.alt as u8) + 4 * (m.control as u8);
    let csi = |final_byte: char| -> Vec<u8> {
        if modifier_code > 1 {
            format!("\x1b[1;{modifier_code}{final_byte}").into_bytes()
        } else if app_cursor {
            format!("\x1bO{final_byte}").into_bytes()
        } else {
            format!("\x1b[{final_byte}").into_bytes()
        }
    };
    let tilde = |code: u8| -> Vec<u8> {
        if modifier_code > 1 {
            format!("\x1b[{code};{modifier_code}~").into_bytes()
        } else {
            format!("\x1b[{code}~").into_bytes()
        }
    };
    let bytes = match key {
        // Shift/Option+Enter insert a newline in Claude Code's prompt.
        "enter" => if m.shift || m.alt { b"\x1b\r".to_vec() } else { b"\r".to_vec() },
        "tab" => if m.shift { b"\x1b[Z".to_vec() } else { b"\t".to_vec() },
        "backspace" => match (m.alt, m.control) {
            (true, _) => b"\x1b\x7f".to_vec(),
            (_, true) => b"\x08".to_vec(),
            _ => b"\x7f".to_vec(),
        },
        "escape" => b"\x1b".to_vec(),
        "up" => csi('A'),
        "down" => csi('B'),
        "right" => csi('C'),
        "left" => csi('D'),
        "home" => csi('H'),
        "end" => csi('F'),
        "pageup" => tilde(5),
        "pagedown" => tilde(6),
        "delete" => tilde(3),
        "insert" => tilde(2),
        "f1" => b"\x1bOP".to_vec(),
        "f2" => b"\x1bOQ".to_vec(),
        "f3" => b"\x1bOR".to_vec(),
        "f4" => b"\x1bOS".to_vec(),
        "f5" => tilde(15),
        "f6" => tilde(17),
        "f7" => tilde(18),
        "f8" => tilde(19),
        "f9" => tilde(20),
        "f10" => tilde(21),
        "f11" => tilde(23),
        "f12" => tilde(24),
        "space" if m.control => vec![0],
        _ => {
            let ch = if key == "space" { ' ' } else {
                let mut chars = key.chars();
                match (chars.next(), chars.next()) {
                    (Some(c), None) => c,
                    _ => return keystroke.key_char.as_ref().map(|s| s.as_bytes().to_vec()),
                }
            };
            if m.control {
                let code = match ch.to_ascii_lowercase() {
                    c @ 'a'..='z' => c as u8 - b'a' + 1,
                    '@' | '2' => 0,
                    '[' | '3' => 27,
                    '\\' | '4' => 28,
                    ']' | '5' => 29,
                    '^' | '6' => 30,
                    '_' | '-' | '7' => 31,
                    '?' | '8' => 127,
                    _ => return None,
                };
                if m.alt { vec![0x1b, code] } else { vec![code] }
            } else if m.alt {
                // Treat Option as Meta (word navigation etc. in Claude's prompt).
                let mut v = vec![0x1b];
                let typed = if m.shift { ch.to_ascii_uppercase() } else { ch };
                v.extend(typed.to_string().as_bytes());
                v
            } else {
                keystroke.key_char.clone().unwrap_or_else(|| ch.to_string()).into_bytes()
            }
        }
    };
    Some(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::Modifiers;

    /// Flood fixture: real ptys running commands that print a lot, fed through
    /// the app's reader thread, bounded queue and batching, and parsed by
    /// alacritty on this thread the way `ingest` does on the UI thread. It
    /// reports how long the UI thread would be blocked per batch.
    ///
    ///     cargo test --release flood_fixture -- --ignored --nocapture
    ///
    /// FLOOD_BYTES sets the size of each flood (default 50 MB).
    /// FLOOD_MAX_BATCH_MS, when set, fails the run if any batch parses slower.
    #[test]
    #[ignore = "slow: run with --ignored --release"]
    fn flood_fixture() {
        let bytes: usize = std::env::var("FLOOD_BYTES").ok().and_then(|s| s.parse().ok()).unwrap_or(50_000_000);
        let budget_ms: Option<f64> = std::env::var("FLOOD_MAX_BATCH_MS").ok().and_then(|s| s.parse().ok());
        let floods = [
            ("yes", format!("yes | head -c {bytes}")),
            (
                "sgr colors",
                format!("yes \"$(printf '\\033[1;38;5;196mred\\033[0m \\033[4;48;2;10;20;30mbg\\033[0m x')\" | head -c {bytes}"),
            ),
            ("one long line", format!("head -c {bytes} /dev/zero | tr '\\0' x")),
            ("random bytes", format!("head -c {bytes} /dev/urandom")),
        ];
        let mut failures = Vec::new();
        eprintln!(
            "\n{:<14} {:>9} {:>8} {:>10} {:>10} {:>10} {:>10} {:>6}",
            "flood", "MB", "batches", "max batch", "p50 ms", "p99 ms", "max ms", "MB/s"
        );
        for (name, script) in floods {
            let stats = run_flood(&script);
            let mut ms: Vec<f64> = stats.batch_ms.clone();
            ms.sort_by(f64::total_cmp);
            let pct = |p: f64| ms[((ms.len() - 1) as f64 * p) as usize];
            let max = *ms.last().unwrap();
            eprintln!(
                "{:<14} {:>9.1} {:>8} {:>8}KB {:>10.2} {:>10.2} {:>10.2} {:>6.0}",
                name,
                stats.total as f64 / 1e6,
                ms.len(),
                stats.max_batch / 1024,
                pct(0.5),
                pct(0.99),
                max,
                stats.total as f64 / 1e6 / stats.elapsed_s,
            );
            // The pty may turn \n into \r\n, so at least `bytes` arrive.
            if stats.total < bytes {
                failures.push(format!("{name}: got {} of {bytes} bytes", stats.total));
            }
            if stats.max_batch > MAX_INGEST_BYTES + 64 * 1024 {
                failures.push(format!("{name}: a batch of {} bytes is over the limit", stats.max_batch));
            }
            if stats.peak_queue > OUTPUT_QUEUE_CHUNKS {
                failures.push(format!("{name}: queue reached {} reads", stats.peak_queue));
            }
            if let Some(budget) = budget_ms.filter(|&b| max > b) {
                failures.push(format!("{name}: a batch took {max:.1} ms, budget {budget} ms"));
            }
        }
        assert!(failures.is_empty(), "{}", failures.join("\n"));
    }

    struct FloodStats {
        total: usize,
        max_batch: usize,
        peak_queue: usize,
        batch_ms: Vec<f64>,
        elapsed_s: f64,
    }

    fn run_flood(script: &str) -> FloodStats {
        let pair = native_pty_system()
            .openpty(PtySize { rows: 30, cols: 100, pixel_width: 0, pixel_height: 0 })
            .unwrap();
        let mut cmd = CommandBuilder::new("sh");
        cmd.args(["-c", script]);
        cmd.env("LC_ALL", "C");
        let mut child = pair.slave.spawn_command(cmd).unwrap();
        drop(pair.slave);
        let (tx, rx) = smol::channel::bounded::<Vec<u8>>(OUTPUT_QUEUE_CHUNKS);
        spawn_pty_reader(pair.master.try_clone_reader().unwrap(), tx);

        let (etx, erx) = mpsc::channel();
        let mut term = Term::new(Config::default(), &TermSize::new(100, 30), Listener(etx));
        let mut parser: Processor = Processor::new();
        let mut stats = FloodStats { total: 0, max_batch: 0, peak_queue: 0, batch_ms: Vec::new(), elapsed_s: 0.0 };
        let start = std::time::Instant::now();
        while let Ok(first) = rx.recv_blocking() {
            stats.peak_queue = stats.peak_queue.max(rx.len() + 1);
            let (bytes, _) = next_batch(first, &rx);
            let t = std::time::Instant::now();
            parser.advance(&mut term, &bytes);
            while erx.try_recv().is_ok() {}
            stats.batch_ms.push(t.elapsed().as_secs_f64() * 1e3);
            stats.total += bytes.len();
            stats.max_batch = stats.max_batch.max(bytes.len());
        }
        stats.elapsed_s = start.elapsed().as_secs_f64();
        let _ = child.wait();
        drop(pair.master);
        stats
    }

    #[test]
    fn output_floods_are_parsed_in_bounded_batches() {
        let (tx, rx) = smol::channel::bounded::<Vec<u8>>(OUTPUT_QUEUE_CHUNKS);
        // A full queue of 64 KB reads, as when something cats a big file.
        for i in 0..OUTPUT_QUEUE_CHUNKS {
            tx.try_send(vec![i as u8; 64 * 1024]).unwrap();
        }
        assert!(tx.try_send(vec![0]).is_err(), "the queue is bounded, so the reader waits");
        let mut batches = Vec::new();
        while let Ok(first) = rx.try_recv() {
            let (bytes, full) = next_batch(first, &rx);
            batches.push((bytes, full));
        }
        let total: usize = batches.iter().map(|(b, _)| b.len()).sum();
        assert_eq!(total, OUTPUT_QUEUE_CHUNKS * 64 * 1024, "nothing lost");
        assert_eq!(batches.len(), 4);
        assert!(batches.iter().all(|(b, full)| b.len() == MAX_INGEST_BYTES && *full));
        // Order is kept across batches.
        let joined: Vec<u8> = batches.into_iter().flat_map(|(b, _)| b).collect();
        assert!(joined.chunks(64 * 1024).enumerate().all(|(i, c)| c.iter().all(|&x| x == i as u8)));

        // Ordinary output (a few small reads) goes in one batch with no yield.
        tx.try_send(b"hello ".to_vec()).unwrap();
        tx.try_send(b"world".to_vec()).unwrap();
        let first = rx.try_recv().unwrap();
        assert_eq!(next_batch(first, &rx), (b"hello world".to_vec(), false));
    }

    fn ks(key: &str, m: Modifiers, ch: Option<&str>) -> Keystroke {
        Keystroke { modifiers: m, key: key.into(), key_char: ch.map(Into::into) }
    }

    #[test]
    fn key_encoding() {
        let none = Modifiers::default();
        let ctrl = Modifiers { control: true, ..Default::default() };
        let shift = Modifiers { shift: true, ..Default::default() };
        let alt = Modifiers { alt: true, ..Default::default() };
        let cmd = Modifiers { platform: true, ..Default::default() };
        assert_eq!(key_to_bytes(&ks("a", none, Some("a")), false), Some(b"a".to_vec()));
        assert_eq!(key_to_bytes(&ks("c", ctrl, None), false), Some(vec![3]));
        assert_eq!(key_to_bytes(&ks("enter", none, None), false), Some(b"\r".to_vec()));
        assert_eq!(key_to_bytes(&ks("enter", shift, None), false), Some(b"\x1b\r".to_vec()));
        assert_eq!(key_to_bytes(&ks("tab", shift, None), false), Some(b"\x1b[Z".to_vec()));
        assert_eq!(key_to_bytes(&ks("up", none, None), false), Some(b"\x1b[A".to_vec()));
        assert_eq!(key_to_bytes(&ks("up", none, None), true), Some(b"\x1bOA".to_vec()));
        assert_eq!(key_to_bytes(&ks("left", alt, None), false), Some(b"\x1b[1;3D".to_vec()));
        assert_eq!(key_to_bytes(&ks("b", alt, Some("∫")), false), Some(b"\x1bb".to_vec()));
        assert_eq!(key_to_bytes(&ks("escape", none, None), false), Some(b"\x1b".to_vec()));
        assert_eq!(key_to_bytes(&ks("v", cmd, None), false), None);
        assert_eq!(key_to_bytes(&ks("A", shift, Some("A")), false), Some(b"A".to_vec()));
    }

    #[test]
    fn url_spans() {
        let find = |text: &str, at: usize| {
            let chars: Vec<char> = text.chars().collect();
            url_span(&chars, at).map(|(s, e)| chars[s..=e].iter().collect::<String>())
        };
        let url = Some("https://example.com/a_(b)".to_string());
        assert_eq!(find("see (https://example.com/a_(b)).", 10), url);
        assert_eq!(find("see (https://example.com/a_(b)).", 30), None);
        assert_eq!(find("see (https://example.com/a_(b)).", 2), None);
        assert_eq!(find("\"http://x.io/?q=1\",", 5), Some("http://x.io/?q=1".to_string()));
        assert_eq!(find("https:// nothing", 3), None);
    }

    #[test]
    fn color_cube() {
        let t = Theme::dark_modern();
        assert_eq!(indexed_color(1, &t), t.ansi[1]);
        assert_eq!(indexed_color(16, &t), rgb_to_hsla(Rgb { r: 0, g: 0, b: 0 }));
        assert_eq!(indexed_color(231, &t), rgb_to_hsla(Rgb { r: 255, g: 255, b: 255 }));
    }

    #[test]
    fn wheel_reports() {
        let sgr = TermMode::MOUSE_REPORT_CLICK | TermMode::SGR_MOUSE;
        assert_eq!(mouse_report(64, 0, 0, sgr), b"\x1b[<64;1;1M");
        assert_eq!(mouse_report(65, 9, 4, sgr), b"\x1b[<65;10;5M");
        assert_eq!(mouse_report(64, 0, 0, TermMode::MOUSE_REPORT_CLICK), b"\x1b[M`!!");
    }

    #[test]
    fn claude_fullscreen_takes_wheel() {
        // The modes Claude Code's fullscreen TUI turns on (captured from v2.1.314).
        let (tx, _rx) = mpsc::channel();
        let mut term = Term::new(Config::default(), &TermSize::new(100, 30), Listener(tx));
        let mut parser: Processor = Processor::new();
        parser.advance(&mut term, b"\x1b[?1049h\x1b[?1000h\x1b[?1002h\x1b[?1003h\x1b[?1006h");
        let mode = *term.mode();
        assert!(mode.contains(TermMode::ALT_SCREEN));
        assert!(mode.intersects(TermMode::MOUSE_MODE));
        assert!(mode.contains(TermMode::SGR_MOUSE));
    }
}
