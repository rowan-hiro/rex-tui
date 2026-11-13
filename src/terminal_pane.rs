use crate::client::TerminalStreamEvent;
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use tokio::task::JoinHandle;

const SCROLLBACK_LINES: usize = 5000;

pub struct TerminalPane {
    pub parser: vt100::Parser,
    pub requested_size: (u16, u16), // (cols, rows)
    pub foreground_process: Option<String>,
    pub title: Option<String>,
    pub exit_code: Option<i32>,
    pub stream_closed: bool,
    pub task_handle: Option<JoinHandle<()>>,
}

impl TerminalPane {
    pub fn new(cols: u16, rows: u16) -> Self {
        let cols = cols.max(2);
        let rows = rows.max(1);
        Self {
            parser: vt100::Parser::new(rows, cols, SCROLLBACK_LINES),
            requested_size: (cols, rows),
            foreground_process: None,
            title: None,
            exit_code: None,
            stream_closed: false,
            task_handle: None,
        }
    }

    pub fn resize_local(&mut self, cols: u16, rows: u16) {
        let cols = cols.max(2);
        let rows = rows.max(1);
        let (cur_rows, cur_cols) = self.parser.screen().size();
        if cur_cols != cols || cur_rows != rows {
            self.parser.set_size(rows, cols);
        }
    }

    pub fn apply_stream_event(&mut self, ev: TerminalStreamEvent) {
        match ev {
            TerminalStreamEvent::InitialVt(bytes) => {
                let (rows, cols) = self.parser.screen().size();
                let mut fresh = vt100::Parser::new(rows, cols, SCROLLBACK_LINES);
                fresh.process(&bytes);
                self.parser = fresh;
            }
            TerminalStreamEvent::Output(bytes) => {
                self.parser.process(&bytes);
            }
            TerminalStreamEvent::Resized { .. } => {}
            TerminalStreamEvent::ChildExited { exit_code } => {
                self.exit_code = Some(exit_code);
            }
            TerminalStreamEvent::Closed => {
                self.stream_closed = true;
            }
        }
    }

    pub fn scroll_up(&mut self, lines: usize) {
        let cur = self.parser.screen().scrollback();
        self.parser.set_scrollback(cur.saturating_add(lines));
    }

    pub fn scroll_down(&mut self, lines: usize) {
        let cur = self.parser.screen().scrollback();
        self.parser.set_scrollback(cur.saturating_sub(lines));
    }

    pub fn reset_scrollback(&mut self) {
        if self.parser.screen().scrollback() != 0 {
            self.parser.set_scrollback(0);
        }
    }

    pub fn application_cursor(&self) -> bool {
        self.parser.screen().application_cursor()
    }

    pub fn bracketed_paste(&self) -> bool {
        self.parser.screen().bracketed_paste()
    }

    /// Returns `(cursor_x, cursor_y)` in absolute terminal coordinates if cursor should be visible.
    pub fn cursor_screen_pos(&self, inner: Rect) -> Option<(u16, u16)> {
        let screen = self.parser.screen();
        if screen.hide_cursor() || screen.scrollback() > 0 || inner.width == 0 || inner.height == 0 {
            return None;
        }
        let (row, col) = screen.cursor_position();
        if row < inner.height && col < inner.width {
            Some((inner.x + col, inner.y + row))
        } else {
            None
        }
    }
}

impl Drop for TerminalPane {
    fn drop(&mut self) {
        if let Some(handle) = self.task_handle.take() {
            handle.abort();
        }
    }
}

/// Renders a `vt100::Screen` directly into a Ratatui `Buffer` within `area`.
pub fn render_vt100_screen(screen: &vt100::Screen, area: Rect, buf: &mut Buffer) {
    if area.width == 0 || area.height == 0 {
        return;
    }

    let (scr_rows, scr_cols) = screen.size();
    let rows = area.height.min(scr_rows);
    let cols = area.width.min(scr_cols);

    for r in 0..rows {
        for c in 0..cols {
            let Some(cell) = screen.cell(r, c) else {
                continue;
            };
            let dst = &mut buf[(area.x + c, area.y + r)];

            if cell.is_wide_continuation() {
                dst.set_symbol("");
                dst.set_style(map_cell_style(cell));
                continue;
            }

            let text = cell.contents();
            if text.is_empty() {
                dst.set_symbol(" ");
            } else {
                dst.set_symbol(text.as_ref());
            }
            dst.set_style(map_cell_style(cell));
        }
    }
}

fn map_cell_style(cell: &vt100::Cell) -> Style {
    let mut style = Style::default()
        .fg(map_vt_color(cell.fgcolor()))
        .bg(map_vt_color(cell.bgcolor()));

    let mut mods = Modifier::empty();
    if cell.bold() {
        mods |= Modifier::BOLD;
    }
    if cell.italic() {
        mods |= Modifier::ITALIC;
    }
    if cell.underline() {
        mods |= Modifier::UNDERLINED;
    }
    if cell.inverse() {
        mods |= Modifier::REVERSED;
    }
    style = style.add_modifier(mods);
    style
}

fn map_vt_color(c: vt100::Color) -> Color {
    match c {
        vt100::Color::Default => Color::Reset,
        vt100::Color::Idx(i) => Color::Indexed(i),
        vt100::Color::Rgb(r, g, b) => Color::Rgb(r, g, b),
    }
}
