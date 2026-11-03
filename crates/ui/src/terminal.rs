use std::collections::VecDeque;
use std::io::{Read, Write};
use std::path::Path;
#[cfg(windows)]
use std::process::{Command, Stdio};
use std::sync::mpsc::{self, Receiver, Sender};

pub const DEFAULT_TERM_FG: u32 = 0xE6EDF3;
pub const DEFAULT_TERM_BG: u32 = 0x00000000;
pub const CURSOR_FG: u32 = 0x0B0C10;
pub const CURSOR_BG: u32 = 0xD4D4DC;
const MAX_SCROLLBACK_LINES: usize = 1000;
const MAX_SPANS_PER_ROW: usize = 16;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TermCell {
    pub ch: char,
    pub fg: u32,
    pub bg: u32,
    pub bold: bool,
    pub dim: bool,
    pub underline: bool,
    pub inverse: bool,
}

impl Default for TermCell {
    fn default() -> Self {
        Self {
            ch: ' ',
            fg: DEFAULT_TERM_FG,
            bg: DEFAULT_TERM_BG,
            bold: false,
            dim: false,
            underline: false,
            inverse: false,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StyledSpan {
    pub text: String,
    pub char_count: usize,
    pub fg: u32,
    pub bg: u32,
    pub bold: bool,
    pub underline: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RenderedTermLine {
    pub spans: Vec<StyledSpan>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ParserState {
    Ground,
    Escape,
    EscapeIntermediate(u8),
    Csi,
    Osc,
    OscEsc,
}

#[derive(Clone, Copy, Debug)]
struct SavedCursorState {
    row: usize,
    col: usize,
    fg: u32,
    bg: u32,
    bold: bool,
    dim: bool,
    underline: bool,
    inverse: bool,
    wrap_pending: bool,
}

impl Default for SavedCursorState {
    fn default() -> Self {
        Self {
            row: 0,
            col: 0,
            fg: DEFAULT_TERM_FG,
            bg: DEFAULT_TERM_BG,
            bold: false,
            dim: false,
            underline: false,
            inverse: false,
            wrap_pending: false,
        }
    }
}

pub struct TerminalEmulator {
    rows: usize,
    cols: usize,
    primary_grid: Vec<Vec<TermCell>>,
    alt_grid: Vec<Vec<TermCell>>,
    scrollback: VecDeque<Vec<TermCell>>,
    using_alt_screen: bool,
    cursor_row: usize,
    cursor_col: usize,
    cursor_visible: bool,
    wrap_pending: bool,
    auto_wrap: bool,
    app_cursor_keys: bool,
    bracketed_paste: bool,
    scroll_top: usize,
    scroll_bottom: usize,
    cur_fg: u32,
    cur_bg: u32,
    cur_bold: bool,
    cur_dim: bool,
    cur_underline: bool,
    cur_inverse: bool,
    last_char: char,
    g0_charset_dec: bool,
    saved_primary: SavedCursorState,
    saved_alt: SavedCursorState,
    state: ParserState,
    csi_buf: Vec<u8>,
    osc_buf: Vec<u8>,
    utf8_buf: Vec<u8>,
    utf8_expected: usize,
    scroll_offset: usize,
    detected_cwd: Option<String>,
    detected_title: Option<String>,
    pending_responses: Vec<Vec<u8>>,
}

impl TerminalEmulator {
    pub fn new(rows: usize, cols: usize) -> Self {
        let r = rows.clamp(4, 200);
        let c = cols.clamp(20, 400);
        let blank_row = vec![TermCell::default(); c];
        Self {
            rows: r,
            cols: c,
            primary_grid: vec![blank_row.clone(); r],
            alt_grid: vec![blank_row; r],
            scrollback: VecDeque::with_capacity(MAX_SCROLLBACK_LINES),
            using_alt_screen: false,
            cursor_row: 0,
            cursor_col: 0,
            cursor_visible: true,
            wrap_pending: false,
            auto_wrap: true,
            app_cursor_keys: false,
            bracketed_paste: false,
            scroll_top: 0,
            scroll_bottom: r - 1,
            cur_fg: DEFAULT_TERM_FG,
            cur_bg: DEFAULT_TERM_BG,
            cur_bold: false,
            cur_dim: false,
            cur_underline: false,
            cur_inverse: false,
            last_char: ' ',
            g0_charset_dec: false,
            saved_primary: SavedCursorState::default(),
            saved_alt: SavedCursorState::default(),
            state: ParserState::Ground,
            csi_buf: Vec::with_capacity(64),
            osc_buf: Vec::with_capacity(256),
            utf8_buf: Vec::with_capacity(4),
            utf8_expected: 0,
            scroll_offset: 0,
            detected_cwd: None,
            detected_title: None,
            pending_responses: Vec::new(),
        }
    }

    pub fn dimensions(&self) -> (usize, usize) {
        (self.rows, self.cols)
    }

    pub fn app_cursor_keys(&self) -> bool {
        self.app_cursor_keys
    }

    pub fn bracketed_paste(&self) -> bool {
        self.bracketed_paste
    }

    pub fn using_alt_screen(&self) -> bool {
        self.using_alt_screen
    }

    pub fn take_detected_cwd(&mut self) -> Option<String> {
        self.detected_cwd.take()
    }

    pub fn take_detected_title(&mut self) -> Option<String> {
        self.detected_title.take()
    }

    pub fn take_pending_responses(&mut self) -> Vec<Vec<u8>> {
        std::mem::take(&mut self.pending_responses)
    }

    pub fn scroll_offset(&self) -> usize {
        self.scroll_offset
    }

    pub fn max_scroll_offset(&self) -> usize {
        if self.using_alt_screen {
            0
        } else {
            self.scrollback.len()
        }
    }

    pub fn scroll_view_by(&mut self, delta_lines: i32) {
        if self.using_alt_screen {
            self.scroll_offset = 0;
            return;
        }
        let max_off = self.scrollback.len() as i32;
        let next = (self.scroll_offset as i32 + delta_lines).clamp(0, max_off);
        self.scroll_offset = next as usize;
    }

    pub fn reset_scroll_to_bottom(&mut self) {
        self.scroll_offset = 0;
    }

    pub fn clear_scrollback_and_screen(&mut self) {
        self.scrollback.clear();
        self.scroll_offset = 0;
        let blank = self.blank_cell();
        for row in self.active_grid_mut() {
            row.fill(blank);
        }
        self.cursor_row = 0;
        self.cursor_col = 0;
        self.wrap_pending = false;
    }

    pub fn resize(&mut self, new_rows: usize, new_cols: usize) {
        let r = new_rows.clamp(4, 200);
        let c = new_cols.clamp(20, 400);
        if r == self.rows && c == self.cols {
            return;
        }
        for row in &mut self.primary_grid {
            row.resize(c, TermCell::default());
        }
        for row in &mut self.alt_grid {
            row.resize(c, TermCell::default());
        }
        for row in &mut self.scrollback {
            row.resize(c, TermCell::default());
        }
        if r > self.rows {
            let extra = r - self.rows;
            for _ in 0..extra {
                if !self.using_alt_screen && !self.scrollback.is_empty() && self.cursor_row > 0 {
                    if let Some(popped) = self.scrollback.pop_back() {
                        self.primary_grid.insert(0, popped);
                        self.cursor_row = (self.cursor_row + 1).min(r - 1);
                    } else {
                        self.primary_grid.push(vec![TermCell::default(); c]);
                    }
                } else {
                    self.primary_grid.push(vec![TermCell::default(); c]);
                }
                self.alt_grid.push(vec![TermCell::default(); c]);
            }
        } else if r < self.rows {
            let excess = self.rows - r;
            for _ in 0..excess {
                if self.cursor_row >= r && !self.primary_grid.is_empty() {
                    let top = self.primary_grid.remove(0);
                    self.push_scrollback_row(top);
                    self.cursor_row = self.cursor_row.saturating_sub(1);
                } else {
                    let _ = self.primary_grid.pop();
                }
                let _ = self.alt_grid.pop();
            }
        }
        self.rows = r;
        self.cols = c;
        self.scroll_top = 0;
        self.scroll_bottom = r - 1;
        self.cursor_row = self.cursor_row.min(r - 1);
        self.cursor_col = self.cursor_col.min(c - 1);
        self.wrap_pending = false;
    }

    pub fn feed_bytes(&mut self, bytes: &[u8]) {
        for &b in bytes {
            self.process_byte(b);
        }
    }

    fn active_grid(&self) -> &[Vec<TermCell>] {
        if self.using_alt_screen {
            &self.alt_grid
        } else {
            &self.primary_grid
        }
    }

    fn active_grid_mut(&mut self) -> &mut Vec<Vec<TermCell>> {
        if self.using_alt_screen {
            &mut self.alt_grid
        } else {
            &mut self.primary_grid
        }
    }

    fn blank_cell(&self) -> TermCell {
        TermCell {
            ch: ' ',
            fg: self.cur_fg,
            bg: self.cur_bg,
            bold: false,
            dim: false,
            underline: false,
            inverse: false,
        }
    }

    fn push_scrollback_row(&mut self, row: Vec<TermCell>) {
        if self.scrollback.len() >= MAX_SCROLLBACK_LINES {
            let _ = self.scrollback.pop_front();
        }
        self.scrollback.push_back(row);
    }

    fn process_byte(&mut self, b: u8) {
        match self.state {
            ParserState::Ground => self.process_ground_byte(b),
            ParserState::Escape => self.process_escape_byte(b),
            ParserState::EscapeIntermediate(inter) => {
                if inter == b'(' && (0x30..=0x7E).contains(&b) {
                    self.g0_charset_dec = b == b'0';
                }
                if (0x30..=0x7E).contains(&b) {
                    self.state = ParserState::Ground;
                }
            }
            ParserState::Csi => self.process_csi_byte(b),
            ParserState::Osc => self.process_osc_byte(b),
            ParserState::OscEsc => {
                if b == b'\\' {
                    self.dispatch_osc();
                }
                self.state = ParserState::Ground;
            }
        }
    }

    fn process_ground_byte(&mut self, b: u8) {
        if self.utf8_expected > 0 {
            if (b & 0xC0) == 0x80 {
                self.utf8_buf.push(b);
                if self.utf8_buf.len() == self.utf8_expected {
                    if let Ok(s) = std::str::from_utf8(&self.utf8_buf) {
                        if let Some(ch) = s.chars().next() {
                            self.put_char(ch);
                        }
                    }
                    self.utf8_buf.clear();
                    self.utf8_expected = 0;
                }
                return;
            }
            self.utf8_buf.clear();
            self.utf8_expected = 0;
        }

        match b {
            0x1B => {
                self.state = ParserState::Escape;
            }
            b'\r' => {
                self.cursor_col = 0;
                self.wrap_pending = false;
            }
            b'\n' | 0x0B | 0x0C => {
                self.line_feed();
            }
            0x08 => {
                self.wrap_pending = false;
                self.cursor_col = self.cursor_col.saturating_sub(1);
            }
            b'\t' => {
                self.wrap_pending = false;
                let next_tab = ((self.cursor_col / 8) + 1) * 8;
                self.cursor_col = next_tab.min(self.cols.saturating_sub(1));
            }
            0x0E => {
                self.g0_charset_dec = true;
            }
            0x0F => {
                self.g0_charset_dec = false;
            }
            0x00..=0x1F | 0x7F => {}
            0x20..=0x7E => {
                self.put_char(b as char);
            }
            0xC2..=0xDF => {
                self.utf8_buf.clear();
                self.utf8_buf.push(b);
                self.utf8_expected = 2;
            }
            0xE0..=0xEF => {
                self.utf8_buf.clear();
                self.utf8_buf.push(b);
                self.utf8_expected = 3;
            }
            0xF0..=0xF4 => {
                self.utf8_buf.clear();
                self.utf8_buf.push(b);
                self.utf8_expected = 4;
            }
            _ => {}
        }
    }

    fn process_escape_byte(&mut self, b: u8) {
        match b {
            b'[' => {
                self.csi_buf.clear();
                self.state = ParserState::Csi;
            }
            b']' => {
                self.osc_buf.clear();
                self.state = ParserState::Osc;
            }
            b'7' => {
                self.save_cursor();
                self.state = ParserState::Ground;
            }
            b'8' => {
                self.restore_cursor();
                self.state = ParserState::Ground;
            }
            b'D' => {
                self.line_feed();
                self.state = ParserState::Ground;
            }
            b'E' => {
                self.cursor_col = 0;
                self.wrap_pending = false;
                self.line_feed();
                self.state = ParserState::Ground;
            }
            b'M' => {
                self.reverse_index();
                self.state = ParserState::Ground;
            }
            b'c' => {
                self.full_reset();
                self.state = ParserState::Ground;
            }
            0x20..=0x2F => {
                self.state = ParserState::EscapeIntermediate(b);
            }
            _ => {
                self.state = ParserState::Ground;
            }
        }
    }

    fn process_csi_byte(&mut self, b: u8) {
        if b == 0x1B {
            self.state = ParserState::Escape;
            return;
        }
        if (0x20..=0x3F).contains(&b) {
            if self.csi_buf.len() < 128 {
                self.csi_buf.push(b);
            }
            return;
        }
        if (0x40..=0x7E).contains(&b) {
            self.dispatch_csi(b);
            self.state = ParserState::Ground;
            return;
        }
        if b == 0x08 {
            self.wrap_pending = false;
            self.cursor_col = self.cursor_col.saturating_sub(1);
        } else if b == b'\r' {
            self.cursor_col = 0;
            self.wrap_pending = false;
        } else if b == b'\n' {
            self.line_feed();
        }
    }

    fn process_osc_byte(&mut self, b: u8) {
        if b == 0x07 {
            self.dispatch_osc();
            self.state = ParserState::Ground;
        } else if b == 0x1B {
            self.state = ParserState::OscEsc;
        } else if self.osc_buf.len() < 1024 {
            self.osc_buf.push(b);
        }
    }

    fn put_char(&mut self, ch: char) {
        let mapped = if self.g0_charset_dec {
            map_dec_special_char(ch)
        } else {
            ch
        };
        self.last_char = mapped;
        self.put_mapped_char(mapped);
    }

    fn put_mapped_char(&mut self, ch: char) {
        if self.scroll_offset > 0 {
            self.scroll_offset = 0;
        }
        if self.wrap_pending && self.auto_wrap {
            self.cursor_col = 0;
            self.wrap_pending = false;
            self.line_feed();
        }
        let r = self.cursor_row.min(self.rows.saturating_sub(1));
        let c = self.cursor_col.min(self.cols.saturating_sub(1));
        let cell = TermCell {
            ch,
            fg: self.cur_fg,
            bg: self.cur_bg,
            bold: self.cur_bold,
            dim: self.cur_dim,
            underline: self.cur_underline,
            inverse: self.cur_inverse,
        };
        let grid = self.active_grid_mut();
        if let Some(row) = grid.get_mut(r) {
            if let Some(slot) = row.get_mut(c) {
                *slot = cell;
            }
        }
        if c + 1 >= self.cols {
            self.wrap_pending = true;
        } else {
            self.cursor_col = c + 1;
        }
    }

    fn line_feed(&mut self) {
        self.wrap_pending = false;
        if self.cursor_row == self.scroll_bottom {
            self.scroll_up_region(self.scroll_top, self.scroll_bottom, 1);
        } else if self.cursor_row + 1 < self.rows {
            self.cursor_row += 1;
        }
    }

    fn reverse_index(&mut self) {
        self.wrap_pending = false;
        if self.cursor_row == self.scroll_top {
            self.scroll_down_region(self.scroll_top, self.scroll_bottom, 1);
        } else if self.cursor_row > 0 {
            self.cursor_row -= 1;
        }
    }

    fn scroll_up_region(&mut self, top: usize, bottom: usize, count: usize) {
        let top = top.min(self.rows.saturating_sub(1));
        let bottom = bottom.min(self.rows.saturating_sub(1));
        if top > bottom {
            return;
        }
        let cols = self.cols;
        let blank = self.blank_cell();
        let n = count.max(1).min(bottom - top + 1);
        for _ in 0..n {
            if !self.using_alt_screen && top == 0 && bottom + 1 == self.rows {
                let removed = self.primary_grid.remove(0);
                self.push_scrollback_row(removed);
                self.primary_grid.push(vec![blank; cols]);
            } else {
                let grid = self.active_grid_mut();
                let _ = grid.remove(top);
                grid.insert(bottom, vec![blank; cols]);
            }
        }
    }

    fn scroll_down_region(&mut self, top: usize, bottom: usize, count: usize) {
        let top = top.min(self.rows.saturating_sub(1));
        let bottom = bottom.min(self.rows.saturating_sub(1));
        if top > bottom {
            return;
        }
        let cols = self.cols;
        let blank = self.blank_cell();
        let n = count.max(1).min(bottom - top + 1);
        let grid = self.active_grid_mut();
        for _ in 0..n {
            let _ = grid.remove(bottom);
            grid.insert(top, vec![blank; cols]);
        }
    }

    fn save_cursor(&mut self) {
        let saved = SavedCursorState {
            row: self.cursor_row,
            col: self.cursor_col,
            fg: self.cur_fg,
            bg: self.cur_bg,
            bold: self.cur_bold,
            dim: self.cur_dim,
            underline: self.cur_underline,
            inverse: self.cur_inverse,
            wrap_pending: self.wrap_pending,
        };
        if self.using_alt_screen {
            self.saved_alt = saved;
        } else {
            self.saved_primary = saved;
        }
    }

    fn restore_cursor(&mut self) {
        let saved = if self.using_alt_screen {
            self.saved_alt
        } else {
            self.saved_primary
        };
        self.cursor_row = saved.row.min(self.rows.saturating_sub(1));
        self.cursor_col = saved.col.min(self.cols.saturating_sub(1));
        self.cur_fg = saved.fg;
        self.cur_bg = saved.bg;
        self.cur_bold = saved.bold;
        self.cur_dim = saved.dim;
        self.cur_underline = saved.underline;
        self.cur_inverse = saved.inverse;
        self.wrap_pending = saved.wrap_pending;
    }

    fn full_reset(&mut self) {
        let (r, c) = (self.rows, self.cols);
        self.using_alt_screen = false;
        self.cursor_row = 0;
        self.cursor_col = 0;
        self.cursor_visible = true;
        self.wrap_pending = false;
        self.auto_wrap = true;
        self.app_cursor_keys = false;
        self.bracketed_paste = false;
        self.scroll_top = 0;
        self.scroll_bottom = r.saturating_sub(1);
        self.cur_fg = DEFAULT_TERM_FG;
        self.cur_bg = DEFAULT_TERM_BG;
        self.cur_bold = false;
        self.cur_dim = false;
        self.cur_underline = false;
        self.cur_inverse = false;
        self.last_char = ' ';
        self.g0_charset_dec = false;
        let blank_row = vec![TermCell::default(); c];
        self.primary_grid = vec![blank_row.clone(); r];
        self.alt_grid = vec![blank_row; r];
    }

    fn dispatch_csi(&mut self, final_byte: u8) {
        let raw = String::from_utf8_lossy(&self.csi_buf).into_owned();
        let is_private = raw.starts_with('?');
        let is_gt = raw.starts_with('>');
        let clean = raw.trim_start_matches(['?', '>', '=', '!']).trim();
        let params: Vec<usize> = if clean.is_empty() {
            Vec::new()
        } else {
            clean
                .split([';', ':'])
                .map(|part| part.parse::<usize>().unwrap_or(0))
                .collect()
        };
        let p0 = params.first().copied().unwrap_or(0);
        let p1 = params.get(1).copied().unwrap_or(0);

        if is_private {
            self.dispatch_private_csi(final_byte, &params);
            return;
        }
        if is_gt {
            if final_byte == b'c' {
                self.pending_responses.push(b"\x1b[>0;276;0c".to_vec());
            }
            return;
        }

        match final_byte {
            b'A' => {
                let n = p0.max(1);
                self.wrap_pending = false;
                if self.cursor_row >= self.scroll_top {
                    self.cursor_row = self.cursor_row.saturating_sub(n).max(self.scroll_top);
                } else {
                    self.cursor_row = self.cursor_row.saturating_sub(n);
                }
            }
            b'B' => {
                let n = p0.max(1);
                self.wrap_pending = false;
                let limit = if self.cursor_row <= self.scroll_bottom {
                    self.scroll_bottom
                } else {
                    self.rows.saturating_sub(1)
                };
                self.cursor_row = (self.cursor_row + n).min(limit);
            }
            b'C' => {
                let n = p0.max(1);
                self.wrap_pending = false;
                self.cursor_col = (self.cursor_col + n).min(self.cols.saturating_sub(1));
            }
            b'D' => {
                let n = p0.max(1);
                self.wrap_pending = false;
                self.cursor_col = self.cursor_col.saturating_sub(n);
            }
            b'E' => {
                let n = p0.max(1);
                self.wrap_pending = false;
                self.cursor_col = 0;
                self.cursor_row = (self.cursor_row + n).min(self.rows.saturating_sub(1));
            }
            b'F' => {
                let n = p0.max(1);
                self.wrap_pending = false;
                self.cursor_col = 0;
                self.cursor_row = self.cursor_row.saturating_sub(n);
            }
            b'G' | b'`' => {
                let col = p0.max(1) - 1;
                self.wrap_pending = false;
                self.cursor_col = col.min(self.cols.saturating_sub(1));
            }
            b'H' | b'f' => {
                let row = p0.max(1) - 1;
                let col = p1.max(1) - 1;
                self.wrap_pending = false;
                self.cursor_row = row.min(self.rows.saturating_sub(1));
                self.cursor_col = col.min(self.cols.saturating_sub(1));
            }
            b'd' => {
                let row = p0.max(1) - 1;
                self.wrap_pending = false;
                self.cursor_row = row.min(self.rows.saturating_sub(1));
            }
            b'b' => {
                let n = p0.max(1).min(self.cols * self.rows);
                let ch = self.last_char;
                for _ in 0..n {
                    self.put_mapped_char(ch);
                }
            }
            b'I' => {
                let n = p0.max(1);
                self.wrap_pending = false;
                let next_tab = ((self.cursor_col / 8) + n) * 8;
                self.cursor_col = next_tab.min(self.cols.saturating_sub(1));
            }
            b'Z' => {
                let n = p0.max(1);
                self.wrap_pending = false;
                if self.cursor_col > 0 {
                    let cur_stop = (self.cursor_col - 1) / 8;
                    let prev_tab = cur_stop.saturating_sub(n.saturating_sub(1)) * 8;
                    self.cursor_col = prev_tab;
                }
            }
            b'J' => {
                self.erase_in_display(p0);
            }
            b'K' => {
                self.erase_in_line(p0);
            }
            b'L' => {
                let n = p0.max(1);
                if self.cursor_row >= self.scroll_top && self.cursor_row <= self.scroll_bottom {
                    self.scroll_down_region(self.cursor_row, self.scroll_bottom, n);
                }
            }
            b'M' => {
                let n = p0.max(1);
                if self.cursor_row >= self.scroll_top && self.cursor_row <= self.scroll_bottom {
                    self.scroll_up_region(self.cursor_row, self.scroll_bottom, n);
                }
            }
            b'@' => {
                let n = p0.max(1);
                let r = self.cursor_row;
                let c = self.cursor_col;
                let cols = self.cols;
                let blank = self.blank_cell();
                if let Some(row) = self.active_grid_mut().get_mut(r) {
                    for _ in 0..n.min(cols.saturating_sub(c)) {
                        let _ = row.pop();
                        row.insert(c, blank);
                    }
                }
            }
            b'P' => {
                let n = p0.max(1);
                let r = self.cursor_row;
                let c = self.cursor_col;
                let cols = self.cols;
                let blank = self.blank_cell();
                if let Some(row) = self.active_grid_mut().get_mut(r) {
                    for _ in 0..n.min(cols.saturating_sub(c)) {
                        if c < row.len() {
                            let _ = row.remove(c);
                            row.push(blank);
                        }
                    }
                }
            }
            b'X' => {
                let n = p0.max(1);
                let r = self.cursor_row;
                let c = self.cursor_col;
                let cols = self.cols;
                let blank = self.blank_cell();
                if let Some(row) = self.active_grid_mut().get_mut(r) {
                    for slot in row.iter_mut().take((c + n).min(cols)).skip(c) {
                        *slot = blank;
                    }
                }
            }
            b'S' => {
                let n = p0.max(1);
                self.scroll_up_region(self.scroll_top, self.scroll_bottom, n);
            }
            b'T' => {
                let n = p0.max(1);
                self.scroll_down_region(self.scroll_top, self.scroll_bottom, n);
            }
            b'r' => {
                let top = if p0 == 0 { 0 } else { p0 - 1 };
                let bot = if p1 == 0 {
                    self.rows.saturating_sub(1)
                } else {
                    (p1 - 1).min(self.rows.saturating_sub(1))
                };
                if top < bot {
                    self.scroll_top = top;
                    self.scroll_bottom = bot;
                    self.cursor_row = 0;
                    self.cursor_col = 0;
                    self.wrap_pending = false;
                }
            }
            b's' => {
                self.save_cursor();
            }
            b'u' => {
                self.restore_cursor();
            }
            b'm' => {
                self.apply_sgr(&params);
            }
            b'c' => {
                self.pending_responses.push(b"\x1b[?1;2c".to_vec());
            }
            b'n' => {
                if p0 == 5 {
                    self.pending_responses.push(b"\x1b[0n".to_vec());
                } else if p0 == 6 {
                    let resp = format!("\x1b[{};{}R", self.cursor_row + 1, self.cursor_col + 1);
                    self.pending_responses.push(resp.into_bytes());
                }
            }
            _ => {}
        }
    }

    fn dispatch_private_csi(&mut self, final_byte: u8, params: &[usize]) {
        let enable = match final_byte {
            b'h' => true,
            b'l' => false,
            _ => return,
        };
        for &mode in params {
            match mode {
                1 => self.app_cursor_keys = enable,
                7 => self.auto_wrap = enable,
                25 => self.cursor_visible = enable,
                47 | 1047 => self.set_alt_screen(enable, false),
                1048 => {
                    if enable {
                        self.save_cursor();
                    } else {
                        self.restore_cursor();
                    }
                }
                1049 => self.set_alt_screen(enable, true),
                2004 => self.bracketed_paste = enable,
                _ => {}
            }
        }
    }

    fn set_alt_screen(&mut self, enable: bool, save_restore_cursor: bool) {
        if enable && !self.using_alt_screen {
            if save_restore_cursor {
                self.save_cursor();
            }
            self.using_alt_screen = true;
            self.scroll_offset = 0;
            let blank = TermCell::default();
            for row in &mut self.alt_grid {
                row.fill(blank);
            }
            self.cursor_row = 0;
            self.cursor_col = 0;
            self.wrap_pending = false;
        } else if !enable && self.using_alt_screen {
            self.using_alt_screen = false;
            self.scroll_top = 0;
            self.scroll_bottom = self.rows.saturating_sub(1);
            if save_restore_cursor {
                self.restore_cursor();
            }
        }
    }

    fn erase_in_display(&mut self, mode: usize) {
        let r = self.cursor_row.min(self.rows.saturating_sub(1));
        let c = self.cursor_col.min(self.cols.saturating_sub(1));
        let blank = self.blank_cell();
        match mode {
            0 => {
                let rows = self.rows;
                let cols = self.cols;
                let grid = self.active_grid_mut();
                if let Some(row) = grid.get_mut(r) {
                    for slot in row.iter_mut().take(cols).skip(c) {
                        *slot = blank;
                    }
                }
                for row in grid.iter_mut().take(rows).skip(r + 1) {
                    row.fill(blank);
                }
            }
            1 => {
                let grid = self.active_grid_mut();
                for row in grid.iter_mut().take(r) {
                    row.fill(blank);
                }
                if let Some(row) = grid.get_mut(r) {
                    for slot in row.iter_mut().take(c + 1) {
                        *slot = blank;
                    }
                }
            }
            2 => {
                for row in self.active_grid_mut() {
                    row.fill(blank);
                }
            }
            3 => {
                self.scrollback.clear();
                self.scroll_offset = 0;
            }
            _ => {}
        }
    }

    fn erase_in_line(&mut self, mode: usize) {
        let r = self.cursor_row.min(self.rows.saturating_sub(1));
        let c = self.cursor_col.min(self.cols.saturating_sub(1));
        let cols = self.cols;
        let blank = self.blank_cell();
        let Some(row) = self.active_grid_mut().get_mut(r) else {
            return;
        };
        match mode {
            0 => {
                for slot in row.iter_mut().take(cols).skip(c) {
                    *slot = blank;
                }
            }
            1 => {
                for slot in row.iter_mut().take((c + 1).min(cols)) {
                    *slot = blank;
                }
            }
            2 => {
                row.fill(blank);
            }
            _ => {}
        }
    }

    fn apply_sgr(&mut self, params: &[usize]) {
        if params.is_empty() {
            self.reset_sgr();
            return;
        }
        let mut idx = 0usize;
        while idx < params.len() {
            let code = params[idx];
            match code {
                0 => self.reset_sgr(),
                1 => self.cur_bold = true,
                2 => self.cur_dim = true,
                4 => self.cur_underline = true,
                7 => self.cur_inverse = true,
                21 | 22 => {
                    self.cur_bold = false;
                    self.cur_dim = false;
                }
                24 => self.cur_underline = false,
                27 => self.cur_inverse = false,
                30..=37 => self.cur_fg = ansi_16_color(code - 30, self.cur_bold),
                39 => self.cur_fg = DEFAULT_TERM_FG,
                40..=47 => self.cur_bg = ansi_16_color(code - 40, false),
                49 => self.cur_bg = DEFAULT_TERM_BG,
                90..=97 => self.cur_fg = ansi_16_color((code - 90) + 8, true),
                100..=107 => self.cur_bg = ansi_16_color((code - 100) + 8, false),
                38 | 48 => {
                    let is_fg = code == 38;
                    let mode = params.get(idx + 1).copied().unwrap_or(0);
                    if mode == 5 {
                        let color_idx = params.get(idx + 2).copied().unwrap_or(0);
                        let rgb = xterm_256_color(color_idx);
                        if is_fg {
                            self.cur_fg = rgb;
                        } else {
                            self.cur_bg = rgb;
                        }
                        idx += 2;
                    } else if mode == 2 {
                        let r = params.get(idx + 2).copied().unwrap_or(0).min(255) as u32;
                        let g = params.get(idx + 3).copied().unwrap_or(0).min(255) as u32;
                        let b = params.get(idx + 4).copied().unwrap_or(0).min(255) as u32;
                        let rgb = 0xFF00_0000 | (r << 16) | (g << 8) | b;
                        if is_fg {
                            self.cur_fg = rgb;
                        } else {
                            self.cur_bg = rgb;
                        }
                        idx += 4;
                    }
                }
                _ => {}
            }
            idx += 1;
        }
    }

    fn reset_sgr(&mut self) {
        self.cur_fg = DEFAULT_TERM_FG;
        self.cur_bg = DEFAULT_TERM_BG;
        self.cur_bold = false;
        self.cur_dim = false;
        self.cur_underline = false;
        self.cur_inverse = false;
    }

    fn dispatch_osc(&mut self) {
        let text = String::from_utf8_lossy(&self.osc_buf);
        let Some((code_str, payload)) = text.split_once(';') else {
            return;
        };
        match code_str {
            "0" | "2" => {
                let clean = payload.trim();
                if !clean.is_empty() {
                    self.detected_title = Some(clean.to_string());
                }
            }
            "7" => {
                if let Some(rest) = payload.strip_prefix("file://") {
                    if let Some(slash_pos) = rest.find('/') {
                        let raw_path = &rest[slash_pos..];
                        let decoded = percent_decode_path(raw_path);
                        if !decoded.is_empty() {
                            self.detected_cwd = Some(decoded);
                        }
                    }
                } else if payload.starts_with('/') || payload.contains(":\\") {
                    self.detected_cwd = Some(payload.trim().to_string());
                }
            }
            "9" => {
                if let Some(path) = payload.strip_prefix("9;") {
                    let clean = path.trim().trim_matches('"');
                    if !clean.is_empty() {
                        self.detected_cwd = Some(clean.to_string());
                    }
                }
            }
            _ => {}
        }
    }

    pub fn render_visible_lines(&self, focused: bool) -> Vec<RenderedTermLine> {
        let mut out = Vec::with_capacity(self.rows);
        let sb_len = self.scrollback.len();
        let eff_offset = if self.using_alt_screen {
            0
        } else {
            self.scroll_offset.min(sb_len)
        };
        let active_grid = self.active_grid();

        for vis_row in 0..self.rows {
            let abs_idx = (sb_len + vis_row).saturating_sub(eff_offset);
            let (row_cells, is_cursor_line) = if abs_idx < sb_len {
                (&self.scrollback[abs_idx], false)
            } else {
                let grid_r = (abs_idx - sb_len).min(self.rows.saturating_sub(1));
                let has_cur = eff_offset == 0 && self.cursor_visible && grid_r == self.cursor_row;
                (&active_grid[grid_r], has_cur)
            };

            let cursor_col = if is_cursor_line {
                let cc = self.cursor_col.min(self.cols.saturating_sub(1));
                let parked_in_alt = self.using_alt_screen
                    && (vis_row + 1 == self.rows || cc > 0)
                    && row_cells.get(cc).is_some_and(|c| c.ch == ' ')
                    && row_cells
                        .first()
                        .is_some_and(|c| c.ch != ' ' && c.ch != ':' && c.ch != '/');
                if parked_in_alt {
                    None
                } else {
                    Some(cc)
                }
            } else {
                None
            };
            let spans = build_styled_spans_for_row(row_cells, cursor_col, focused);
            out.push(RenderedTermLine { spans });
        }
        out
    }
}

fn map_dec_special_char(ch: char) -> char {
    match ch {
        'j' => '┘',
        'k' => '┐',
        'l' => '┌',
        'm' => '└',
        'n' => '┼',
        'q' => '─',
        't' => '├',
        'u' => '┤',
        'v' => '┴',
        'w' => '┬',
        'x' => '│',
        'a' => '▒',
        '`' => '◆',
        'f' => '°',
        'g' => '±',
        '~' => '·',
        _ => ch,
    }
}

fn build_styled_spans_for_row(
    cells: &[TermCell],
    cursor_col: Option<usize>,
    focused: bool,
) -> Vec<StyledSpan> {
    let mut last_non_blank = 0usize;
    for (idx, cell) in cells.iter().enumerate() {
        let has_style = cell.ch != ' '
            || cell.bg != DEFAULT_TERM_BG
            || cell.inverse
            || cell.underline
            || cursor_col == Some(idx);
        if has_style {
            last_non_blank = idx + 1;
        }
    }
    if let Some(cc) = cursor_col {
        last_non_blank = last_non_blank.max(cc + 1);
    }
    if last_non_blank == 0 {
        return Vec::new();
    }

    let mut spans: Vec<StyledSpan> = Vec::with_capacity(8);
    for (idx, cell) in cells.iter().enumerate().take(last_non_blank) {
        let is_cur = cursor_col == Some(idx);
        let (fg, bg, bold) = if is_cur {
            let (cur_fg, cur_bg) = if focused {
                (CURSOR_FG, CURSOR_BG)
            } else {
                (DEFAULT_TERM_FG, 0x52525E)
            };
            (0xFF00_0000 | cur_fg, 0xFF00_0000 | cur_bg, true)
        } else {
            let mut eff_fg = if cell.fg == DEFAULT_TERM_FG {
                0xFF00_0000 | DEFAULT_TERM_FG
            } else {
                0xFF00_0000 | (cell.fg & 0x00FF_FFFF)
            };
            let mut eff_bg = if cell.bg == DEFAULT_TERM_BG {
                0
            } else {
                0xFF00_0000 | (cell.bg & 0x00FF_FFFF)
            };
            if cell.inverse {
                let bg_for_fg = if eff_bg == 0 { 0xFF0B_0C10 } else { eff_bg };
                std::mem::swap(&mut eff_fg, &mut eff_bg);
                eff_fg = bg_for_fg;
            }
            if cell.dim && !cell.bold {
                eff_fg = dim_rgb(eff_fg);
            }
            (eff_fg, eff_bg, cell.bold)
        };

        let display_ch = if cell.ch == ' ' { '\u{00A0}' } else { cell.ch };
        let span_len = spans.len();
        if let Some(last) = spans.last_mut() {
            let same_style = !is_cur
                && last.fg == fg
                && last.bg == bg
                && last.bold == bold
                && last.underline == cell.underline;
            if same_style && span_len < MAX_SPANS_PER_ROW {
                last.text.push(display_ch);
                last.char_count += 1;
                continue;
            }
            if span_len >= MAX_SPANS_PER_ROW && !is_cur {
                last.text.push(display_ch);
                last.char_count += 1;
                continue;
            }
        }
        spans.push(StyledSpan {
            text: display_ch.to_string(),
            char_count: 1,
            fg,
            bg,
            bold,
            underline: cell.underline,
        });
    }
    spans
}

fn dim_rgb(argb: u32) -> u32 {
    let r = ((argb >> 16) & 0xFF) * 2 / 3;
    let g = ((argb >> 8) & 0xFF) * 2 / 3;
    let b = (argb & 0xFF) * 2 / 3;
    0xFF00_0000 | (r << 16) | (g << 8) | b
}

fn ansi_16_color(idx: usize, bright: bool) -> u32 {
    let eff = if bright && idx < 8 { idx + 8 } else { idx };
    let rgb = match eff {
        0 => 0x282A36,
        1 => 0xFF5F56,
        2 => 0x4E9A5E,
        3 => 0xF1FA8C,
        4 => 0x4CC2FF,
        5 => 0xBD93F9,
        6 => 0x6E92B8,
        7 => 0xE4E4EC,
        8 => 0x6D6D78,
        9 => 0xFF6E67,
        10 => 0x5AF78E,
        11 => 0xF4F99D,
        12 => 0x58A6FF,
        13 => 0xFF92D0,
        14 => 0x8BE9FD,
        _ => 0xFFFFFF,
    };
    0xFF00_0000 | rgb
}

fn xterm_256_color(idx: usize) -> u32 {
    if idx < 16 {
        return ansi_16_color(idx, idx >= 8);
    }
    if idx < 232 {
        let c = idx - 16;
        let r_idx = c / 36;
        let g_idx = (c / 6) % 6;
        let b_idx = c % 6;
        let scale = [0u32, 95, 135, 175, 215, 255];
        let r = scale[r_idx.min(5)];
        let g = scale[g_idx.min(5)];
        let b = scale[b_idx.min(5)];
        return 0xFF00_0000 | (r << 16) | (g << 8) | b;
    }
    let gray = (8 + (idx.saturating_sub(232).min(23) as u32) * 10).min(255);
    0xFF00_0000 | (gray << 16) | (gray << 8) | gray
}

fn percent_decode_path(raw: &str) -> String {
    let bytes = raw.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0usize;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            if let (Some(hi), Some(lo)) = (hex_val(bytes[i + 1]), hex_val(bytes[i + 2])) {
                out.push((hi << 4) | lo);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn hex_val(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

pub fn encode_terminal_key_event(
    text: &str,
    key_name: &str,
    ctrl: bool,
    alt: bool,
    shift: bool,
    app_cursor: bool,
) -> Option<Vec<u8>> {
    match key_name {
        "return" => return Some(vec![b'\r']),
        "backspace" => return Some(vec![0x7F]),
        "tab" => {
            if shift {
                return Some(b"\x1b[Z".to_vec());
            }
            return Some(vec![b'\t']);
        }
        "escape" => return Some(vec![0x1B]),
        "up" => {
            let seq = if app_cursor {
                &b"\x1bOA"[..]
            } else {
                &b"\x1b[A"[..]
            };
            return Some(seq.to_vec());
        }
        "down" => {
            let seq = if app_cursor {
                &b"\x1bOB"[..]
            } else {
                &b"\x1b[B"[..]
            };
            return Some(seq.to_vec());
        }
        "right" => {
            let seq = if app_cursor {
                &b"\x1bOC"[..]
            } else {
                &b"\x1b[C"[..]
            };
            return Some(seq.to_vec());
        }
        "left" => {
            let seq = if app_cursor {
                &b"\x1bOD"[..]
            } else {
                &b"\x1b[D"[..]
            };
            return Some(seq.to_vec());
        }
        "home" => {
            let seq = if app_cursor {
                &b"\x1bOH"[..]
            } else {
                &b"\x1b[H"[..]
            };
            return Some(seq.to_vec());
        }
        "end" => {
            let seq = if app_cursor {
                &b"\x1bOF"[..]
            } else {
                &b"\x1b[F"[..]
            };
            return Some(seq.to_vec());
        }
        "delete" => return Some(b"\x1b[3~".to_vec()),
        "pageup" => return Some(b"\x1b[5~".to_vec()),
        "pagedown" => return Some(b"\x1b[6~".to_vec()),
        _ => {}
    }

    if ctrl {
        if let Some(ch) = text.chars().next() {
            let lower = ch.to_ascii_lowercase();
            if lower.is_ascii_lowercase() {
                let code = (lower as u8) - b'a' + 1;
                return Some(vec![code]);
            }
            match ch {
                '\x01'..='\x1A' => return Some(vec![ch as u8]),
                '[' | '3' => return Some(vec![0x1B]),
                '\\' | '4' => return Some(vec![0x1C]),
                ']' | '5' => return Some(vec![0x1D]),
                '^' | '6' => return Some(vec![0x1E]),
                '_' | '/' | '7' => return Some(vec![0x1F]),
                ' ' | '@' | '2' => return Some(vec![0x00]),
                _ => {}
            }
        }
    }

    if text.is_empty() {
        return None;
    }
    let ch = text.chars().next()?;
    if ch == '\r' || ch == '\n' {
        return Some(vec![b'\r']);
    }
    if ch == '\x08' || ch == '\x7F' {
        return Some(vec![0x7F]);
    }
    if ch == '\t' {
        return Some(vec![b'\t']);
    }
    if ch.is_control() && !ctrl {
        return None;
    }
    let mut payload = Vec::with_capacity(text.len() + 1);
    if alt {
        payload.push(0x1B);
    }
    payload.extend_from_slice(text.as_bytes());
    Some(payload)
}

#[derive(Clone, Debug)]
pub struct RemoteSshSpec {
    pub conn_name: String,
    pub host: String,
    pub port: u16,
    pub username: String,
    pub key_path: Option<String>,
    pub secret: Option<String>,
    pub remote_dir: String,
}

impl RemoteSshSpec {
    pub fn build_ssh_args(&self) -> Vec<String> {
        let mut args = vec![
            "-tt".to_string(),
            "-o".to_string(),
            "StrictHostKeyChecking=accept-new".to_string(),
            "-o".to_string(),
            "ConnectTimeout=8".to_string(),
            "-p".to_string(),
            self.port.to_string(),
        ];
        if let Some(key) = &self.key_path {
            let trimmed = key.trim();
            if !trimmed.is_empty() {
                let expanded = if let Some(rest) = trimmed.strip_prefix("~/") {
                    let home = std::env::var("HOME").unwrap_or_default();
                    format!("{}/{rest}", home.trim_end_matches('/'))
                } else {
                    trimmed.to_string()
                };
                if Path::new(&expanded).exists() {
                    args.push("-i".to_string());
                    args.push(expanded);
                }
            }
        }
        let host_clean = self.host.trim();
        let user_clean = self.username.trim();
        let target = if user_clean.is_empty() {
            host_clean.to_string()
        } else {
            format!("{user_clean}@{host_clean}")
        };
        args.push(target);
        let clean_dir = if self.remote_dir.trim().is_empty() {
            "/"
        } else {
            self.remote_dir.trim()
        };
        let esc_dir = clean_dir.replace('\'', "'\\''");
        let sh_expr = "${SHELL:-/bin/sh}";
        let remote_cmd = format!("cd -- '{esc_dir}' 2>/dev/null; exec \"{sh_expr}\" -l");
        args.push(remote_cmd);
        args
    }
}

pub struct NativeShellSession {
    stdin_tx: Sender<Vec<u8>>,
    stdout_rx: Receiver<Vec<u8>>,
    shell_name: String,
    #[cfg(unix)]
    master_fd: i32,
    #[cfg(unix)]
    child_pid: i32,
    #[cfg(windows)]
    conpty_handle: isize,
}

impl NativeShellSession {
    pub fn spawn(initial_cwd: &str, rows: u16, cols: u16) -> Option<Self> {
        let start_dir = if !initial_cwd.contains("://") && Path::new(initial_cwd).is_dir() {
            initial_cwd
        } else if cfg!(windows) {
            "C:\\"
        } else {
            "/"
        };
        #[cfg(unix)]
        {
            Self::spawn_unix_pty(start_dir, rows, cols)
        }
        #[cfg(windows)]
        {
            Self::spawn_windows_powershell(start_dir, rows, cols)
        }
    }

    pub fn spawn_remote_ssh(spec: &RemoteSshSpec, rows: u16, cols: u16) -> Option<Self> {
        if spec.host.trim().is_empty() {
            return None;
        }
        #[cfg(unix)]
        {
            Self::spawn_unix_ssh_pty(spec, rows, cols)
        }
        #[cfg(windows)]
        {
            Self::spawn_windows_ssh(spec, rows, cols)
        }
    }

    pub fn shell_name(&self) -> &str {
        &self.shell_name
    }

    pub fn send_bytes(&self, bytes: &[u8]) -> bool {
        self.stdin_tx.send(bytes.to_vec()).is_ok()
    }

    pub fn try_recv_bytes(&self) -> Option<Vec<u8>> {
        self.stdout_rx.try_recv().ok()
    }

    pub fn resize_pty(&self, rows: u16, cols: u16) {
        #[cfg(unix)]
        unsafe {
            let ws = libc::winsize {
                ws_row: rows.max(4),
                ws_col: cols.max(20),
                ws_xpixel: 0,
                ws_ypixel: 0,
            };
            let _ = libc::ioctl(self.master_fd, libc::TIOCSWINSZ, &ws);
        }
        #[cfg(windows)]
        {
            resize_win_conpty(self.conpty_handle, rows.max(4), cols.max(20));
        }
    }

    pub fn is_idle_at_prompt(&self) -> bool {
        #[cfg(unix)]
        unsafe {
            let fg_pgid = libc::tcgetpgrp(self.master_fd);
            fg_pgid <= 0 || fg_pgid == self.child_pid
        }
        #[cfg(not(unix))]
        {
            true
        }
    }

    pub fn query_live_cwd(&self) -> Option<String> {
        #[cfg(target_os = "linux")]
        {
            let proc_cwd = format!("/proc/{}/cwd", self.child_pid);
            std::fs::read_link(proc_cwd)
                .ok()
                .and_then(|p| p.to_str().map(str::to_string))
        }
        #[cfg(not(target_os = "linux"))]
        {
            None
        }
    }

    pub fn sync_cwd_if_idle(&self, dir_path: &str) {
        if dir_path.contains("://") || !Path::new(dir_path).is_dir() {
            return;
        }
        if !self.is_idle_at_prompt() {
            return;
        }
        #[cfg(unix)]
        {
            let escaped = dir_path.replace('\'', "'\\''");
            let cmd = format!("\x15 cd -- '{escaped}'\r");
            let _ = self.send_bytes(cmd.as_bytes());
        }
        #[cfg(windows)]
        {
            let escaped = dir_path.replace('\'', "''");
            let cmd = format!("Set-Location -LiteralPath '{escaped}'\r\n");
            let _ = self.send_bytes(cmd.as_bytes());
        }
    }

    pub fn sync_remote_cwd_if_idle(&self, remote_dir: &str) {
        let clean = if remote_dir.trim().is_empty() {
            "/"
        } else {
            remote_dir.trim()
        };
        if !self.is_idle_at_prompt() {
            return;
        }
        let escaped = clean.replace('\'', "'\\''");
        let cmd = format!("\x15 cd -- '{escaped}'\r");
        let _ = self.send_bytes(cmd.as_bytes());
    }

    #[cfg(unix)]
    fn spawn_unix_ssh_pty(spec: &RemoteSshSpec, rows: u16, cols: u16) -> Option<Self> {
        let ws = libc::winsize {
            ws_row: rows.max(4),
            ws_col: cols.max(20),
            ws_xpixel: 0,
            ws_ypixel: 0,
        };

        let (master_fd, slave_fd) = unsafe {
            let mfd = libc::posix_openpt(libc::O_RDWR | libc::O_NOCTTY);
            if mfd < 0 {
                return None;
            }
            if libc::grantpt(mfd) != 0 || libc::unlockpt(mfd) != 0 {
                libc::close(mfd);
                return None;
            }
            let pts_ptr = libc::ptsname(mfd);
            if pts_ptr.is_null() {
                libc::close(mfd);
                return None;
            }
            let sfd = libc::open(pts_ptr, libc::O_RDWR | libc::O_NOCTTY);
            if sfd < 0 {
                libc::close(mfd);
                return None;
            }
            let _ = libc::ioctl(mfd, libc::TIOCSWINSZ, &ws);
            (mfd, sfd)
        };

        let mut raw_argv = vec!["ssh".to_string()];
        raw_argv.extend(spec.build_ssh_args());
        let mut c_args = Vec::with_capacity(raw_argv.len());
        for arg in &raw_argv {
            c_args.push(std::ffi::CString::new(arg.as_str()).ok()?);
        }
        let mut argv_ptrs: Vec<*const std::ffi::c_char> =
            c_args.iter().map(|s| s.as_ptr()).collect();
        argv_ptrs.push(std::ptr::null());

        let pid = unsafe { libc::fork() };
        if pid < 0 {
            unsafe {
                libc::close(master_fd);
                libc::close(slave_fd);
            }
            return None;
        }

        if pid == 0 {
            unsafe {
                libc::close(master_fd);
                let _ = libc::setsid();
                let _ = libc::ioctl(slave_fd, libc::TIOCSCTTY as _, 0);
                let _ = libc::dup2(slave_fd, 0);
                let _ = libc::dup2(slave_fd, 1);
                let _ = libc::dup2(slave_fd, 2);
                if slave_fd > 2 {
                    libc::close(slave_fd);
                }

                let env_vars: [(&std::ffi::CStr, &std::ffi::CStr); 7] = [
                    (c"TERM", c"xterm-256color"),
                    (c"COLORTERM", c"truecolor"),
                    (c"CLICOLOR", c"1"),
                    (c"HISTCONTROL", c"ignoreboth"),
                    (c"PROMPT_EOL_MARK", c""),
                    (c"TERM_PROGRAM", c"Apple_Terminal"),
                    (c"LC_MESSAGES", c"C"),
                ];
                for (k, v) in env_vars {
                    let _ = libc::setenv(k.as_ptr(), v.as_ptr(), 1);
                }

                libc::execvp(c"ssh".as_ptr(), argv_ptrs.as_ptr());
                libc::_exit(1);
            }
        }

        unsafe {
            libc::close(slave_fd);
        }

        let read_fd = unsafe { libc::dup(master_fd) };
        let write_fd = unsafe { libc::dup(master_fd) };
        if read_fd < 0 || write_fd < 0 {
            unsafe {
                if read_fd >= 0 {
                    libc::close(read_fd);
                }
                if write_fd >= 0 {
                    libc::close(write_fd);
                }
                libc::close(master_fd);
            }
            return None;
        }

        let mut reader_file =
            unsafe { <std::fs::File as std::os::fd::FromRawFd>::from_raw_fd(read_fd) };
        let mut writer_file =
            unsafe { <std::fs::File as std::os::fd::FromRawFd>::from_raw_fd(write_fd) };

        let (stdin_tx, stdin_rx) = mpsc::channel::<Vec<u8>>();
        let (stdout_tx, stdout_rx) = mpsc::channel::<Vec<u8>>();
        let auth_tx = stdin_tx.clone();
        let secret_pw = spec.secret.clone().filter(|s| !s.is_empty());

        std::thread::spawn(move || {
            while let Ok(bytes) = stdin_rx.recv() {
                if writer_file.write_all(&bytes).is_err() {
                    break;
                }
                let _ = writer_file.flush();
            }
        });

        std::thread::spawn(move || {
            let mut buf = [0u8; 4096];
            let mut sent_pw = secret_pw.is_none();
            let mut strip_echo_newline = false;
            let mut tail = Vec::<u8>::new();
            loop {
                match reader_file.read(&mut buf) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => {
                        let mut slice = &buf[..n];
                        if strip_echo_newline {
                            while !slice.is_empty() && (slice[0] == b'\r' || slice[0] == b'\n') {
                                slice = &slice[1..];
                            }
                            strip_echo_newline = false;
                            if slice.is_empty() {
                                continue;
                            }
                        }
                        if !sent_pw {
                            tail.extend_from_slice(slice);
                            if tail.len() > 512 {
                                let drain_len = tail.len() - 512;
                                tail.drain(..drain_len);
                            }
                            let low: Vec<u8> =
                                tail.iter().map(|b| b.to_ascii_lowercase()).collect();
                            let has_pw_prompt = low.windows(9).any(|w| w == b"password:")
                                || low.windows(10).any(|w| w == b"passphrase");
                            if has_pw_prompt {
                                if let Some(pw) = &secret_pw {
                                    let payload = format!("{pw}\n").into_bytes();
                                    let _ = auth_tx.send(payload);
                                }
                                sent_pw = true;
                                strip_echo_newline = true;
                                tail.clear();
                                continue;
                            }
                        }
                        if stdout_tx.send(slice.to_vec()).is_err() {
                            break;
                        }
                    }
                }
            }
        });

        Some(Self {
            stdin_tx,
            stdout_rx,
            shell_name: "ssh".to_string(),
            master_fd,
            child_pid: pid,
        })
    }

    #[cfg(unix)]
    fn spawn_unix_pty(start_dir: &str, rows: u16, cols: u16) -> Option<Self> {
        let (shell_bin, shell_label) = detect_unix_shell();
        let ws = libc::winsize {
            ws_row: rows.max(4),
            ws_col: cols.max(20),
            ws_xpixel: 0,
            ws_ypixel: 0,
        };

        let (master_fd, slave_fd) = unsafe {
            let mfd = libc::posix_openpt(libc::O_RDWR | libc::O_NOCTTY);
            if mfd < 0 {
                return None;
            }
            if libc::grantpt(mfd) != 0 || libc::unlockpt(mfd) != 0 {
                libc::close(mfd);
                return None;
            }
            let pts_ptr = libc::ptsname(mfd);
            if pts_ptr.is_null() {
                libc::close(mfd);
                return None;
            }
            let sfd = libc::open(pts_ptr, libc::O_RDWR | libc::O_NOCTTY);
            if sfd < 0 {
                libc::close(mfd);
                return None;
            }
            let _ = libc::ioctl(mfd, libc::TIOCSWINSZ, &ws);
            (mfd, sfd)
        };

        let c_shell = std::ffi::CString::new(shell_bin.as_str()).ok()?;
        let c_arg0 = std::ffi::CString::new(shell_bin.as_str()).ok()?;
        let c_arg_i = std::ffi::CString::new("-i").ok()?;
        let c_dir = std::ffi::CString::new(start_dir).ok()?;

        let pid = unsafe { libc::fork() };
        if pid < 0 {
            unsafe {
                libc::close(master_fd);
                libc::close(slave_fd);
            }
            return None;
        }

        if pid == 0 {
            unsafe {
                libc::close(master_fd);
                let _ = libc::setsid();
                let _ = libc::ioctl(slave_fd, libc::TIOCSCTTY as _, 0);
                let _ = libc::dup2(slave_fd, 0);
                let _ = libc::dup2(slave_fd, 1);
                let _ = libc::dup2(slave_fd, 2);
                if slave_fd > 2 {
                    libc::close(slave_fd);
                }
                let _ = libc::chdir(c_dir.as_ptr());

                let env_vars: [(&std::ffi::CStr, &std::ffi::CStr); 6] = [
                    (c"TERM", c"xterm-256color"),
                    (c"COLORTERM", c"truecolor"),
                    (c"CLICOLOR", c"1"),
                    (c"HISTCONTROL", c"ignoreboth"),
                    (c"PROMPT_EOL_MARK", c""),
                    (c"TERM_PROGRAM", c"Apple_Terminal"),
                ];
                for (k, v) in env_vars {
                    let _ = libc::setenv(k.as_ptr(), v.as_ptr(), 1);
                }

                let argv = [c_arg0.as_ptr(), c_arg_i.as_ptr(), std::ptr::null()];
                libc::execvp(c_shell.as_ptr(), argv.as_ptr());
                libc::_exit(1);
            }
        }

        unsafe {
            libc::close(slave_fd);
        }

        let read_fd = unsafe { libc::dup(master_fd) };
        let write_fd = unsafe { libc::dup(master_fd) };
        if read_fd < 0 || write_fd < 0 {
            unsafe {
                if read_fd >= 0 {
                    libc::close(read_fd);
                }
                if write_fd >= 0 {
                    libc::close(write_fd);
                }
                libc::close(master_fd);
            }
            return None;
        }

        let mut reader_file =
            unsafe { <std::fs::File as std::os::fd::FromRawFd>::from_raw_fd(read_fd) };
        let mut writer_file =
            unsafe { <std::fs::File as std::os::fd::FromRawFd>::from_raw_fd(write_fd) };

        let (stdin_tx, stdin_rx) = mpsc::channel::<Vec<u8>>();
        let (stdout_tx, stdout_rx) = mpsc::channel::<Vec<u8>>();

        std::thread::spawn(move || {
            while let Ok(bytes) = stdin_rx.recv() {
                if writer_file.write_all(&bytes).is_err() {
                    break;
                }
                let _ = writer_file.flush();
            }
        });

        std::thread::spawn(move || {
            let mut buf = [0u8; 4096];
            loop {
                match reader_file.read(&mut buf) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => {
                        if stdout_tx.send(buf[..n].to_vec()).is_err() {
                            break;
                        }
                    }
                }
            }
        });

        Some(Self {
            stdin_tx,
            stdout_rx,
            shell_name: shell_label,
            master_fd,
            child_pid: pid,
        })
    }

    #[cfg(windows)]
    fn spawn_windows_ssh(spec: &RemoteSshSpec, rows: u16, cols: u16) -> Option<Self> {
        let mut cmd_parts = vec!["ssh.exe".to_string()];
        for arg in spec.build_ssh_args() {
            if arg.contains(' ') {
                let esc = arg.replace('"', "\\\"");
                cmd_parts.push(format!("\"{esc}\""));
            } else {
                cmd_parts.push(arg);
            }
        }
        let cmd_line = cmd_parts.join(" ");
        if let Some(conpty) = try_spawn_win_conpty(&cmd_line, "ssh", "C:\\", rows, cols) {
            return Some(conpty);
        }
        Self::spawn_windows_powershell("C:\\", rows, cols)
    }

    #[cfg(windows)]
    fn spawn_windows_powershell(start_dir: &str, rows: u16, cols: u16) -> Option<Self> {
        let (ps_bin, ps_label) = detect_windows_powershell();
        let cmd_str = format!("{ps_bin} -NoLogo");
        if let Some(conpty) = try_spawn_win_conpty(&cmd_str, &ps_label, start_dir, rows, cols) {
            return Some(conpty);
        }
        let init_script = concat!(
            "$OutputEncoding = [Console]::InputEncoding = [Console]::OutputEncoding = ",
            "[System.Text.UTF8Encoding]::new($false); ",
            "function prompt { ",
            "$p = (Get-Location).Path; ",
            "[Console]::Write(\"`e]9;9;$p`a`e[32mPS `e[36m$p`e[0m> \"); ",
            "return ' ' }"
        );
        let mut child = Command::new(&ps_bin)
            .args(["-NoLogo", "-NoExit", "-Command", init_script])
            .current_dir(start_dir)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .ok()?;

        let mut child_in = child.stdin.take()?;
        let mut child_out = child.stdout.take()?;
        let mut child_err = child.stderr.take()?;

        let (stdin_tx, stdin_rx) = mpsc::channel::<Vec<u8>>();
        let (stdout_tx, stdout_rx) = mpsc::channel::<Vec<u8>>();
        let stdout_tx_err = stdout_tx.clone();
        let echo_tx = stdout_tx.clone();
        let cur_dir_init = start_dir.replace('/', "\\");

        let init_prompt = format!("\x1b[32mPS \x1b[36m{cur_dir_init}\x1b[0m> ").into_bytes();
        let _ = stdout_tx.send(init_prompt);

        std::thread::spawn(move || {
            let mut line_buf = String::new();
            let mut history: Vec<String> = Vec::new();
            let mut hist_pos = 0usize;
            while let Ok(bytes) = stdin_rx.recv() {
                if bytes == b"\x1b[A" {
                    if !history.is_empty() && hist_pos > 0 {
                        let old_len = line_buf.chars().count();
                        hist_pos -= 1;
                        line_buf = history[hist_pos].clone();
                        let erase = "\x08".repeat(old_len);
                        let msg = format!("{erase}\x1b[K{line_buf}");
                        let _ = echo_tx.send(msg.into_bytes());
                    }
                    continue;
                }
                if bytes == b"\x1b[B" {
                    if hist_pos < history.len() {
                        let old_len = line_buf.chars().count();
                        hist_pos += 1;
                        line_buf = if hist_pos < history.len() {
                            history[hist_pos].clone()
                        } else {
                            String::new()
                        };
                        let erase = "\x08".repeat(old_len);
                        let msg = format!("{erase}\x1b[K{line_buf}");
                        let _ = echo_tx.send(msg.into_bytes());
                    }
                    continue;
                }
                for &b in &bytes {
                    match b {
                        b'\r' | b'\n' => {
                            let _ = echo_tx.send(b"\r\n".to_vec());
                            let trimmed = line_buf.trim().to_string();
                            if !trimmed.is_empty() && history.last() != Some(&trimmed) {
                                history.push(trimmed);
                            }
                            hist_pos = history.len();
                            let cmd_line = format!("{line_buf}\r\n");
                            line_buf.clear();
                            if child_in.write_all(cmd_line.as_bytes()).is_err() {
                                return;
                            }
                            let _ = child_in.flush();
                        }
                        0x08 | 0x7F => {
                            if line_buf.pop().is_some() {
                                let _ = echo_tx.send(b"\x08\x1b[K".to_vec());
                            }
                        }
                        0x03 => {
                            line_buf.clear();
                            let _ = echo_tx.send(b"^C\r\n".to_vec());
                            let _ = child_in.write_all(b"\r\n");
                            let _ = child_in.flush();
                        }
                        0x0C => {
                            let _ = echo_tx.send(b"\x1b[2J\x1b[H".to_vec());
                            let _ = child_in.write_all(b"\r\n");
                            let _ = child_in.flush();
                        }
                        0x20..=0x7E => {
                            line_buf.push(b as char);
                            let _ = echo_tx.send(vec![b]);
                        }
                        _ => {}
                    }
                }
            }
            let _ = child.kill();
        });

        std::thread::spawn(move || {
            let mut buf = [0u8; 4096];
            loop {
                match child_out.read(&mut buf) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => {
                        let normalized = normalize_lf_to_crlf(&buf[..n]);
                        if stdout_tx.send(normalized).is_err() {
                            break;
                        }
                    }
                }
            }
        });

        std::thread::spawn(move || {
            let mut buf = [0u8; 4096];
            loop {
                match child_err.read(&mut buf) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => {
                        let mut colored = b"\x1b[31m".to_vec();
                        colored.extend_from_slice(&normalize_lf_to_crlf(&buf[..n]));
                        colored.extend_from_slice(b"\x1b[0m");
                        if stdout_tx_err.send(colored).is_err() {
                            break;
                        }
                    }
                }
            }
        });

        Some(Self {
            stdin_tx,
            stdout_rx,
            shell_name: ps_label,
            conpty_handle: 0,
        })
    }
}

#[cfg(unix)]
impl Drop for NativeShellSession {
    fn drop(&mut self) {
        unsafe {
            if self.master_fd >= 0 {
                libc::close(self.master_fd);
            }
            if self.child_pid > 0 {
                let _ = libc::kill(self.child_pid, libc::SIGHUP);
            }
        }
    }
}

#[cfg(windows)]
impl Drop for NativeShellSession {
    fn drop(&mut self) {
        close_win_conpty(self.conpty_handle);
    }
}

#[cfg(unix)]
fn detect_unix_shell() -> (String, String) {
    if let Ok(env_sh) = std::env::var("SHELL") {
        let trimmed = env_sh.trim();
        if !trimmed.is_empty() && Path::new(trimmed).is_file() {
            let name = Path::new(trimmed)
                .file_name()
                .and_then(|s| s.to_str())
                .unwrap_or("sh")
                .to_string();
            return (trimmed.to_string(), name);
        }
    }
    let candidates = if cfg!(target_os = "macos") {
        ["/bin/zsh", "/bin/bash", "/bin/sh"]
    } else {
        ["/bin/bash", "/usr/bin/bash", "/bin/zsh"]
    };
    for cand in candidates {
        if Path::new(cand).is_file() {
            let name = Path::new(cand)
                .file_name()
                .and_then(|s| s.to_str())
                .unwrap_or("sh")
                .to_string();
            return (cand.to_string(), name);
        }
    }
    ("/bin/sh".to_string(), "sh".to_string())
}

#[cfg(windows)]
fn detect_windows_powershell() -> (String, String) {
    let pwsh_ok = Command::new("pwsh.exe")
        .arg("-Version")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|s| s.success());
    if pwsh_ok {
        return ("pwsh.exe".to_string(), "pwsh".to_string());
    }
    ("powershell.exe".to_string(), "powershell".to_string())
}

#[cfg(windows)]
fn normalize_lf_to_crlf(bytes: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(bytes.len() + 16);
    let mut prev = 0u8;
    for &b in bytes {
        if b == b'\n' && prev != b'\r' {
            out.push(b'\r');
        }
        out.push(b);
        prev = b;
    }
    out
}

#[cfg(windows)]
#[repr(C)]
#[allow(dead_code)]
struct WinStartupInfoW {
    cb: u32,
    lp_reserved: *mut u16,
    lp_desktop: *mut u16,
    lp_title: *mut u16,
    dw_x: u32,
    dw_y: u32,
    dw_x_size: u32,
    dw_y_size: u32,
    dw_x_count_chars: u32,
    dw_y_count_chars: u32,
    dw_fill_attribute: u32,
    dw_flags: u32,
    w_show_window: u16,
    cb_reserved2: u16,
    lp_reserved2: *mut u8,
    h_std_input: isize,
    h_std_output: isize,
    h_std_error: isize,
}

#[cfg(windows)]
#[repr(C)]
#[allow(dead_code)]
struct WinStartupInfoExW {
    startup_info: WinStartupInfoW,
    lp_attribute_list: *mut u8,
}

#[cfg(windows)]
#[repr(C)]
#[allow(dead_code)]
struct WinProcessInformation {
    h_process: isize,
    h_thread: isize,
    dw_process_id: u32,
    dw_thread_id: u32,
}

#[cfg(windows)]
extern "system" {
    fn CreatePipe(
        h_read_pipe: *mut isize,
        h_write_pipe: *mut isize,
        lp_pipe_attributes: *const u8,
        n_size: u32,
    ) -> i32;
    fn CloseHandle(h_object: isize) -> i32;
    fn GetModuleHandleA(lp_module_name: *const std::ffi::c_char) -> isize;
    fn GetProcAddress(h_module: isize, lp_proc_name: *const std::ffi::c_char) -> *const ();
}

#[cfg(windows)]
type CreateProcessWFn = unsafe extern "system" fn(
    *const u16,
    *mut u16,
    *const u8,
    *const u8,
    i32,
    u32,
    *const u8,
    *const u16,
    *const WinStartupInfoExW,
    *mut WinProcessInformation,
) -> i32;

#[cfg(windows)]
type CreatePseudoConsoleFn = unsafe extern "system" fn(u32, isize, isize, u32, *mut isize) -> i32;
#[cfg(windows)]
type ResizePseudoConsoleFn = unsafe extern "system" fn(isize, u32) -> i32;
#[cfg(windows)]
type ClosePseudoConsoleFn = unsafe extern "system" fn(isize);
#[cfg(windows)]
type InitAttrListFn = unsafe extern "system" fn(*mut u8, u32, u32, *mut usize) -> i32;
#[cfg(windows)]
type UpdateAttrFn =
    unsafe extern "system" fn(*mut u8, u32, usize, *const u8, usize, *mut u8, *mut usize) -> i32;
#[cfg(windows)]
type DeleteAttrListFn = unsafe extern "system" fn(*mut u8);

#[cfg(windows)]
fn try_spawn_win_conpty(
    cmd_str: &str,
    ps_label: &str,
    start_dir: &str,
    rows: u16,
    cols: u16,
) -> Option<NativeShellSession> {
    unsafe {
        let k32 = GetModuleHandleA(c"kernel32.dll".as_ptr());
        if k32 == 0 {
            return None;
        }
        let p_create = GetProcAddress(k32, c"CreatePseudoConsole".as_ptr());
        let p_init = GetProcAddress(k32, c"InitializeProcThreadAttributeList".as_ptr());
        let p_update = GetProcAddress(k32, c"UpdateProcThreadAttribute".as_ptr());
        let p_delete = GetProcAddress(k32, c"DeleteProcThreadAttributeList".as_ptr());
        let p_create_proc = GetProcAddress(k32, c"CreateProcessW".as_ptr());
        if p_create.is_null()
            || p_init.is_null()
            || p_update.is_null()
            || p_delete.is_null()
            || p_create_proc.is_null()
        {
            return None;
        }
        let create_pc: CreatePseudoConsoleFn = std::mem::transmute(p_create);
        let init_attr: InitAttrListFn = std::mem::transmute(p_init);
        let update_attr: UpdateAttrFn = std::mem::transmute(p_update);
        let delete_attr: DeleteAttrListFn = std::mem::transmute(p_delete);
        let create_proc: CreateProcessWFn = std::mem::transmute(p_create_proc);

        let (mut pty_in_read, mut pty_in_write) = (0isize, 0isize);
        let (mut pty_out_read, mut pty_out_write) = (0isize, 0isize);
        if CreatePipe(&mut pty_in_read, &mut pty_in_write, std::ptr::null(), 0) == 0 {
            return None;
        }
        if CreatePipe(&mut pty_out_read, &mut pty_out_write, std::ptr::null(), 0) == 0 {
            CloseHandle(pty_in_read);
            CloseHandle(pty_in_write);
            return None;
        }

        let coord: u32 = ((u32::from(rows.max(4))) << 16) | u32::from(cols.max(20));
        let mut h_pc: isize = 0;
        let hr = create_pc(coord, pty_in_read, pty_out_write, 0, &mut h_pc);
        CloseHandle(pty_in_read);
        CloseHandle(pty_out_write);
        if hr != 0 || h_pc == 0 {
            CloseHandle(pty_in_write);
            CloseHandle(pty_out_read);
            return None;
        }

        let mut attr_size: usize = 0;
        let _ = init_attr(std::ptr::null_mut(), 1, 0, &mut attr_size);
        let mut attr_buf = vec![0u8; attr_size.max(64)];
        if init_attr(attr_buf.as_mut_ptr(), 1, 0, &mut attr_size) == 0 {
            close_win_conpty(h_pc);
            CloseHandle(pty_in_write);
            CloseHandle(pty_out_read);
            return None;
        }

        const PROC_THREAD_ATTRIBUTE_PSEUDOCONSOLE: usize = 0x0002_0016;
        let upd_ok = update_attr(
            attr_buf.as_mut_ptr(),
            0,
            PROC_THREAD_ATTRIBUTE_PSEUDOCONSOLE,
            h_pc as *const u8,
            std::mem::size_of::<isize>(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
        );
        if upd_ok == 0 {
            delete_attr(attr_buf.as_mut_ptr());
            close_win_conpty(h_pc);
            CloseHandle(pty_in_write);
            CloseHandle(pty_out_read);
            return None;
        }

        let mut si_ex: WinStartupInfoExW = std::mem::zeroed();
        si_ex.startup_info.cb = std::mem::size_of::<WinStartupInfoExW>() as u32;
        si_ex.lp_attribute_list = attr_buf.as_mut_ptr();

        let mut cmd_w: Vec<u16> = cmd_str.encode_utf16().chain(std::iter::once(0)).collect();
        let dir_w: Vec<u16> = start_dir.encode_utf16().chain(std::iter::once(0)).collect();
        let mut pi: WinProcessInformation = std::mem::zeroed();
        const EXTENDED_STARTUPINFO_PRESENT: u32 = 0x0008_0000;

        let ok = create_proc(
            std::ptr::null(),
            cmd_w.as_mut_ptr(),
            std::ptr::null(),
            std::ptr::null(),
            0,
            EXTENDED_STARTUPINFO_PRESENT,
            std::ptr::null(),
            dir_w.as_ptr(),
            &si_ex,
            &mut pi,
        );
        delete_attr(attr_buf.as_mut_ptr());
        if ok == 0 {
            close_win_conpty(h_pc);
            CloseHandle(pty_in_write);
            CloseHandle(pty_out_read);
            return None;
        }
        CloseHandle(pi.h_thread);
        CloseHandle(pi.h_process);

        use std::os::windows::io::FromRawHandle;
        let in_ptr = pty_in_write as *mut std::ffi::c_void;
        let out_ptr = pty_out_read as *mut std::ffi::c_void;
        let mut writer_file = std::fs::File::from_raw_handle(in_ptr);
        let mut reader_file = std::fs::File::from_raw_handle(out_ptr);

        let (stdin_tx, stdin_rx) = mpsc::channel::<Vec<u8>>();
        let (stdout_tx, stdout_rx) = mpsc::channel::<Vec<u8>>();

        std::thread::spawn(move || {
            while let Ok(bytes) = stdin_rx.recv() {
                if writer_file.write_all(&bytes).is_err() {
                    break;
                }
                let _ = writer_file.flush();
            }
        });

        std::thread::spawn(move || {
            let mut buf = [0u8; 4096];
            loop {
                match reader_file.read(&mut buf) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => {
                        if stdout_tx.send(buf[..n].to_vec()).is_err() {
                            break;
                        }
                    }
                }
            }
        });

        Some(NativeShellSession {
            stdin_tx,
            stdout_rx,
            shell_name: ps_label.to_string(),
            conpty_handle: h_pc,
        })
    }
}

#[cfg(windows)]
fn resize_win_conpty(h_pc: isize, rows: u16, cols: u16) {
    if h_pc == 0 {
        return;
    }
    unsafe {
        let k32 = GetModuleHandleA(c"kernel32.dll".as_ptr());
        if k32 != 0 {
            let p_resize = GetProcAddress(k32, c"ResizePseudoConsole".as_ptr());
            if !p_resize.is_null() {
                let resize_fn: ResizePseudoConsoleFn = std::mem::transmute(p_resize);
                let coord: u32 = ((u32::from(rows)) << 16) | u32::from(cols);
                let _ = resize_fn(h_pc, coord);
            }
        }
    }
}

#[cfg(windows)]
fn close_win_conpty(h_pc: isize) {
    if h_pc == 0 {
        return;
    }
    unsafe {
        let k32 = GetModuleHandleA(c"kernel32.dll".as_ptr());
        if k32 != 0 {
            let p_close = GetProcAddress(k32, c"ClosePseudoConsole".as_ptr());
            if !p_close.is_null() {
                let close_fn: ClosePseudoConsoleFn = std::mem::transmute(p_close);
                close_fn(h_pc);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_vt100_readline_prompt_cursor_and_sgr_colors() {
        let mut emu = TerminalEmulator::new(24, 80);
        emu.feed_bytes(b"\x1b]7;file://macbook/Users/dimi/Downloads%20Test\x07");
        assert_eq!(
            emu.take_detected_cwd().as_deref(),
            Some("/Users/dimi/Downloads Test")
        );

        emu.feed_bytes(b"\x1b[1;32mdimi@mac\x1b[0m:\x1b[34m~/Downloads\x1b[0m$ ls -la");
        let lines = emu.render_visible_lines(true);
        let row0_text: String = lines[0]
            .spans
            .iter()
            .map(|s| s.text.replace('\u{00A0}', " "))
            .collect();
        assert_eq!(row0_text, "dimi@mac:~/Downloads$ ls -la ");

        emu.feed_bytes(b"\x08\x08\x08\x08\x1b[K");
        let lines_after_bs = emu.render_visible_lines(true);
        let row0_after: String = lines_after_bs[0]
            .spans
            .iter()
            .map(|s| s.text.replace('\u{00A0}', " "))
            .collect();
        assert_eq!(row0_after, "dimi@mac:~/Downloads$ ls ");
    }

    #[test]
    fn test_vt100_alternate_screen_buffer_restores_primary_grid() {
        let mut emu = TerminalEmulator::new(10, 40);
        emu.feed_bytes(b"Primary shell prompt$ ");
        assert!(!emu.using_alt_screen());

        emu.feed_bytes(b"\x1b[?1049h\x1b[1;1Hhtop fullscreen TUI");
        assert!(emu.using_alt_screen());
        let alt_lines = emu.render_visible_lines(true);
        let alt_row0: String = alt_lines[0]
            .spans
            .iter()
            .map(|s| s.text.replace('\u{00A0}', " "))
            .collect();
        assert!(alt_row0.starts_with("htop fullscreen TUI"));

        emu.feed_bytes(b"\x1b[?1049l");
        assert!(!emu.using_alt_screen());
        let restored = emu.render_visible_lines(true);
        let rest_row0: String = restored[0]
            .spans
            .iter()
            .map(|s| s.text.replace('\u{00A0}', " "))
            .collect();
        assert_eq!(rest_row0, "Primary shell prompt$  ");
    }

    #[test]
    fn test_key_encoding_for_shell_navigation_and_signals() {
        assert_eq!(
            encode_terminal_key_event("", "tab", false, false, false, false),
            Some(vec![b'\t'])
        );
        assert_eq!(
            encode_terminal_key_event("", "up", false, false, false, false),
            Some(b"\x1b[A".to_vec())
        );
        assert_eq!(
            encode_terminal_key_event("", "up", false, false, false, true),
            Some(b"\x1bOA".to_vec())
        );
        assert_eq!(
            encode_terminal_key_event("c", "", true, false, false, false),
            Some(vec![0x03])
        );
        assert_eq!(
            encode_terminal_key_event("r", "", true, false, false, false),
            Some(vec![0x12])
        );
    }

    #[test]
    fn test_vt100_rep_repeat_spaces_and_dec_box_drawing() {
        let mut emu = TerminalEmulator::new(10, 60);
        emu.feed_bytes(b"PID COMMAND \x1b[4b\x1b[7m%CPU\x1b(B\x1b[m TIME\r\n");
        emu.feed_bytes(b"\x1b(0lqqqk\x1b(B");
        let lines = emu.render_visible_lines(true);
        let row0: String = lines[0]
            .spans
            .iter()
            .map(|s| s.text.replace('\u{00A0}', " "))
            .collect();
        assert_eq!(row0, "PID COMMAND     %CPU TIME");
        let total_chars: usize = lines[0].spans.iter().map(|s| s.char_count).sum();
        assert_eq!(total_chars, 25);

        let row1: String = lines[1]
            .spans
            .iter()
            .map(|s| s.text.replace('\u{00A0}', " "))
            .collect();
        assert!(row1.starts_with("┌───┐"));
    }

    #[test]
    fn test_remote_ssh_spec_builds_interactive_pty_args() {
        let spec = RemoteSshSpec {
            conn_name: "Vibe VM".to_string(),
            host: "192.168.8.110".to_string(),
            port: 22,
            username: "root".to_string(),
            key_path: None,
            secret: Some("secret123".to_string()),
            remote_dir: "/var/log".to_string(),
        };
        let args = spec.build_ssh_args();
        assert!(args.contains(&"-tt".to_string()));
        assert!(args.contains(&"root@192.168.8.110".to_string()));
        let last = args.last().cloned().unwrap_or_default();
        assert!(last.contains("cd -- '/var/log'"));
    }
}
