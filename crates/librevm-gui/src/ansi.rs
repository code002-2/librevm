//! A terminal emulator.
//!
//! The console pane used to append whatever came out of the guest straight into a
//! text view. That is fine for a kernel log, and useless for anything that draws: a
//! shell's colours arrive as escape sequences and get shown as rubbish, `clear`
//! prints its own name, and anything that moves the cursor -- a progress bar, `top`,
//! an editor -- turns into a wall of text.
//!
//! So the bytes are interpreted instead. This keeps a grid of cells with their
//! attributes, moves a cursor around it, and pushes lines into a scrollback as they
//! scroll off the top. The GTK side then renders the grid.
//!
//! Supported, which is enough for a BusyBox shell and most CLI tools:
//!
//!   * C0 controls: BEL BS HT LF VT FF CR
//!   * CSI: cursor movement, erase in line/display, insert/delete lines and
//!     characters, scroll up/down, set scrolling region, save/restore cursor,
//!     show/hide cursor, SGR, device status report
//!   * OSC: title (accepted and discarded)
//!   * DEC private modes: autowrap, cursor visible, origin, application cursor keys
//!
//! Not supported, and deliberately: alternate screen, mouse reporting, sixel,
//! bracketed paste, DEC line drawing. A BusyBox guest does not need them.

use std::collections::VecDeque;

/// How many lines of history to keep above the screen.
const SCROLLBACK: usize = 2000;

/// A colour: either one of the eight ANSI slots, a bright variant, or the default.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Color {
    #[default]
    Default,
    Indexed(u8),
    Rgb(u8, u8, u8),
}

/// The drawing attributes in effect for newly written cells.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Pen {
    pub fg: Color,
    pub bg: Color,
    pub bold: bool,
    pub dim: bool,
    pub italic: bool,
    pub underline: bool,
    pub reverse: bool,
    pub strike: bool,
}

/// One character cell.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Cell {
    pub ch: char,
    pub pen: Pen,
}

impl Default for Cell {
    fn default() -> Self {
        Cell {
            ch: ' ',
            pen: Pen::default(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum State {
    Ground,
    Escape,
    Csi,
    CsiIntermediate,
    Osc,
    OscEscape,
}

/// The parser and the screen.
pub struct Terminal {
    /// The visible screen: `rows` lines of `cols` cells.
    screen: Vec<Vec<Cell>>,
    /// Lines that have scrolled off the top, oldest first.
    scrollback: VecDeque<Vec<Cell>>,
    rows: usize,
    cols: usize,
    /// Cursor position, zero based.
    cx: usize,
    cy: usize,
    pen: Pen,
    /// The scrolling region, as inclusive zero-based rows.
    scroll_top: usize,
    scroll_bottom: usize,
    /// Autowrap: when set, writing past the last column wraps.
    wrap: bool,
    /// Set when the cursor is parked in the last column and the next character
    /// should wrap first. Real terminals defer the wrap so that a line exactly as
    /// wide as the screen does not scroll twice.
    pending_wrap: bool,
    cursor_visible: bool,
    origin_mode: bool,
    application_cursor: bool,
    saved: Option<(usize, usize, Pen)>,

    state: State,
    /// Numeric CSI parameters collected so far.
    params: Vec<i32>,
    /// The private marker of a CSI sequence, e.g. `?` in `ESC[?25h`.
    private: Option<char>,
    /// The intermediate byte of a CSI sequence, if any.
    intermediate: Option<char>,
    /// A multibyte character being assembled across `feed` calls.
    utf8_pending: Utf8Pending,
}

/// Anything the application has to write back to the guest.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Reply {
    /// A device status report answer, e.g. the cursor position.
    Bytes(Vec<u8>),
}

impl Terminal {
    pub fn new(rows: usize, cols: usize) -> Self {
        let rows = rows.max(1);
        let cols = cols.max(1);
        Terminal {
            screen: vec![vec![Cell::default(); cols]; rows],
            scrollback: VecDeque::new(),
            rows,
            cols,
            cx: 0,
            cy: 0,
            pen: Pen::default(),
            scroll_top: 0,
            scroll_bottom: rows - 1,
            wrap: true,
            pending_wrap: false,
            cursor_visible: true,
            origin_mode: false,
            application_cursor: false,
            saved: None,
            state: State::Ground,
            params: Vec::new(),
            private: None,
            intermediate: None,
            utf8_pending: Utf8Pending::default(),
        }
    }

    pub fn rows(&self) -> usize {
        self.rows
    }

    pub fn cols(&self) -> usize {
        self.cols
    }

    pub fn cursor_visible(&self) -> bool {
        self.cursor_visible
    }

    pub fn cursor(&self) -> (usize, usize) {
        (self.cx, self.cy)
    }

    pub fn screen(&self) -> &[Vec<Cell>] {
        &self.screen
    }

    pub fn scrollback(&self) -> &VecDeque<Vec<Cell>> {
        &self.scrollback
    }

    /// Total lines available for display: history plus the screen.
    pub fn total_lines(&self) -> usize {
        self.scrollback.len() + self.rows
    }

    /// A line by index across the whole history, or `None` if out of range.
    pub fn line(&self, index: usize) -> Option<&Vec<Cell>> {
        let sb = self.scrollback.len();
        if index < sb {
            self.scrollback.get(index)
        } else {
            self.screen.get(index - sb)
        }
    }

    /// Resize. Content is preserved as far as the two sizes overlap.
    pub fn resize(&mut self, rows: usize, cols: usize) {
        let rows = rows.max(1);
        let cols = cols.max(1);
        if rows == self.rows && cols == self.cols {
            return;
        }

        // width first: keep the left-hand part of each line
        for line in self.screen.iter_mut() {
            line.resize(cols, Cell::default());
            line.truncate(cols);
        }
        self.cols = cols;

        if rows < self.rows {
            // the top lines fall into the scrollback, as a real terminal would
            let drop = self.rows - rows;
            for _ in 0..drop {
                let line = self.screen.remove(0);
                self.scrollback.push_back(line);
            }
            while self.scrollback.len() > SCROLLBACK {
                self.scrollback.pop_front();
            }
        } else {
            for _ in 0..(rows - self.rows) {
                self.screen.push(vec![Cell::default(); self.cols]);
            }
        }
        self.rows = rows;

        self.scroll_top = 0;
        self.scroll_bottom = self.rows - 1;
        self.cx = self.cx.min(self.cols - 1);
        self.cy = self.cy.min(self.rows - 1);
        self.pending_wrap = false;
    }

    /// Feed bytes from the guest. Returns anything to write back.
    pub fn feed(&mut self, data: &[u8]) -> Vec<Reply> {
        let mut replies = Vec::new();
        for &b in data {
            self.byte(b, &mut replies);
        }
        replies
    }

    /// The visible screen plus history as plain text, for tests and for copying.
    pub fn dump(&self) -> String {
        let mut out = String::new();
        for i in 0..self.total_lines() {
            if let Some(line) = self.line(i) {
                let text: String = line.iter().map(|c| c.ch).collect();
                out.push_str(text.trim_end());
                out.push('\n');
            }
        }
        out
    }

    // -- the parser ---------------------------------------------------------

    fn byte(&mut self, b: u8, replies: &mut Vec<Reply>) {
        match self.state {
            State::Ground => self.ground(b, replies),
            State::Escape => self.escape(b, replies),
            State::Csi | State::CsiIntermediate => self.csi(b, replies),
            State::Osc => {
                // consume until BEL or ST
                if b == 0x07 {
                    self.state = State::Ground;
                } else if b == 0x1b {
                    self.state = State::OscEscape;
                }
            }
            State::OscEscape => {
                // ESC \ ends it; anything else is part of the string
                self.state = if b == b'\\' { State::Ground } else { State::Osc };
            }
        }
    }

    fn ground(&mut self, b: u8, replies: &mut Vec<Reply>) {
        match b {
            0x1b => {
                self.state = State::Escape;
                self.params.clear();
                self.private = None;
                self.intermediate = None;
            }
            0x07 => {} // bell: nothing to do
            0x08 => {
                // backspace
                self.pending_wrap = false;
                self.cx = self.cx.saturating_sub(1);
            }
            0x09 => {
                // tab, to the next multiple of eight
                self.pending_wrap = false;
                let next = ((self.cx / 8) + 1) * 8;
                self.cx = next.min(self.cols - 1);
            }
            0x0a | 0x0b | 0x0c => {
                // line feed, vertical tab, form feed
                self.pending_wrap = false;
                self.line_feed();
            }
            0x0d => {
                self.pending_wrap = false;
                self.cx = 0;
            }
            _ => {
                if b < 0x20 {
                    return; // other C0 controls are ignored
                }
                self.print(b, replies);
            }
        }
    }

    fn escape(&mut self, b: u8, _replies: &mut Vec<Reply>) {
        match b {
            b'[' => {
                self.state = State::Csi;
                self.params.clear();
                self.private = None;
                self.intermediate = None;
            }
            b']' => {
                self.state = State::Osc;
            }
            b'(' | b')' | b'*' | b'+' => {
                // character set selection: consume the designator and ignore it
                self.state = State::Escape;
                self.intermediate = Some(b as char);
            }
            b'7' => {
                self.saved = Some((self.cx, self.cy, self.pen));
                self.state = State::Ground;
            }
            b'8' => {
                if let Some((x, y, pen)) = self.saved {
                    self.cx = x.min(self.cols - 1);
                    self.cy = y.min(self.rows - 1);
                    self.pen = pen;
                }
                self.state = State::Ground;
            }
            b'D' => {
                // index: down one line, scrolling if needed
                self.line_feed();
                self.state = State::Ground;
            }
            b'M' => {
                // reverse index
                if self.cy == self.scroll_top {
                    self.scroll_down();
                } else {
                    self.cy = self.cy.saturating_sub(1);
                }
                self.state = State::Ground;
            }
            b'E' => {
                self.cx = 0;
                self.line_feed();
                self.state = State::Ground;
            }
            b'c' => {
                // full reset
                *self = Terminal::new(self.rows, self.cols);
            }
            b'=' | b'>' => {
                // keypad modes: ignored
                self.state = State::Ground;
            }
            _ => {
                self.state = State::Ground;
            }
        }
    }

    fn csi(&mut self, b: u8, replies: &mut Vec<Reply>) {
        match b {
            b'0'..=b'9' => {
                let last = self.params.pop().unwrap_or(0);
                let digit = (b - b'0') as i32;
                self.params.push(last.saturating_mul(10).saturating_add(digit));
                self.state = State::Csi;
            }
            b';' => {
                if self.params.is_empty() {
                    self.params.push(0);
                }
                self.params.push(0);
                self.state = State::Csi;
            }
            b'?' | b'<' | b'=' | b'>' => {
                self.private = Some(b as char);
                self.state = State::Csi;
            }
            b' ' | b'!' | b'"' | b'#' | b'$' | b'\'' => {
                self.intermediate = Some(b as char);
                self.state = State::CsiIntermediate;
            }
            0x40..=0x7e => {
                self.dispatch(b, replies);
                self.state = State::Ground;
                self.params.clear();
                self.private = None;
                self.intermediate = None;
            }
            _ => {
                // a control byte inside a CSI sequence is executed
                self.state = State::Ground;
            }
        }
    }

    /// A CSI parameter, with the "missing means default" rule terminals use.
    fn param(&self, index: usize, default: i32) -> i32 {
        match self.params.get(index) {
            Some(&0) | None => default,
            Some(&v) => v,
        }
    }

    fn dispatch(&mut self, final_byte: u8, replies: &mut Vec<Reply>) {
        let private = self.private;
        self.pending_wrap = false;

        match final_byte {
            // -- cursor movement -------------------------------------------
            b'A' => {
                let n = self.param(0, 1) as usize;
                let top = if self.cy >= self.scroll_top { self.scroll_top } else { 0 };
                self.cy = self.cy.saturating_sub(n).max(top);
            }
            b'B' => {
                let n = self.param(0, 1) as usize;
                let bottom = if self.cy <= self.scroll_bottom { self.scroll_bottom } else { self.rows - 1 };
                self.cy = (self.cy + n).min(bottom);
            }
            b'C' => {
                let n = self.param(0, 1) as usize;
                self.cx = (self.cx + n).min(self.cols - 1);
            }
            b'D' => {
                let n = self.param(0, 1) as usize;
                self.cx = self.cx.saturating_sub(n);
            }
            b'E' => {
                let n = self.param(0, 1) as usize;
                self.cx = 0;
                self.cy = (self.cy + n).min(self.rows - 1);
            }
            b'F' => {
                let n = self.param(0, 1) as usize;
                self.cx = 0;
                self.cy = self.cy.saturating_sub(n);
            }
            b'G' | b'`' => {
                let n = self.param(0, 1).max(1) as usize;
                self.cx = (n - 1).min(self.cols - 1);
            }
            b'd' => {
                let n = self.param(0, 1).max(1) as usize;
                let row = if self.origin_mode { self.scroll_top + n - 1 } else { n - 1 };
                self.cy = row.min(self.rows - 1);
            }
            b'H' | b'f' => {
                let row = self.param(0, 1).max(1) as usize;
                let col = self.param(1, 1).max(1) as usize;
                let base = if self.origin_mode { self.scroll_top } else { 0 };
                self.cx = (col - 1).min(self.cols - 1);
                self.cy = (base + row - 1).min(self.rows - 1);
            }

            // -- erasing ---------------------------------------------------
            b'J' => {
                match self.param(0, 0) {
                    0 => {
                        // cursor to end
                        self.erase_cells(self.cy, self.cx, self.cols);
                        for y in (self.cy + 1)..self.rows {
                            self.erase_cells(y, 0, self.cols);
                        }
                    }
                    1 => {
                        for y in 0..self.cy {
                            self.erase_cells(y, 0, self.cols);
                        }
                        self.erase_cells(self.cy, 0, self.cx + 1);
                    }
                    2 | 3 => {
                        for y in 0..self.rows {
                            self.erase_cells(y, 0, self.cols);
                        }
                    }
                    _ => {}
                }
            }
            b'K' => {
                match self.param(0, 0) {
                    0 => self.erase_cells(self.cy, self.cx, self.cols),
                    1 => self.erase_cells(self.cy, 0, self.cx + 1),
                    2 => self.erase_cells(self.cy, 0, self.cols),
                    _ => {}
                }
            }
            b'X' => {
                let n = self.param(0, 1) as usize;
                let from = self.cx;
                let to = (self.cx + n).min(self.cols);
                self.erase_cells(self.cy, from, to);
            }

            // -- inserting and deleting ------------------------------------
            b'@' => {
                let n = self.param(0, 1) as usize;
                let line = &mut self.screen[self.cy];
                for _ in 0..n.min(self.cols - self.cx) {
                    line.insert(self.cx, Cell::default());
                    line.truncate(self.cols);
                }
            }
            b'P' => {
                let n = self.param(0, 1) as usize;
                let line = &mut self.screen[self.cy];
                for _ in 0..n.min(self.cols - self.cx) {
                    if self.cx < line.len() {
                        line.remove(self.cx);
                    }
                    line.push(Cell::default());
                }
            }
            b'L' => {
                // insert lines at the cursor, inside the scrolling region
                if self.cy >= self.scroll_top && self.cy <= self.scroll_bottom {
                    let n = self.param(0, 1) as usize;
                    for _ in 0..n {
                        self.screen.insert(self.cy, vec![Cell::default(); self.cols]);
                        self.screen.remove(self.scroll_bottom + 1);
                    }
                }
            }
            b'M' => {
                if self.cy >= self.scroll_top && self.cy <= self.scroll_bottom {
                    let n = self.param(0, 1) as usize;
                    for _ in 0..n {
                        self.screen.remove(self.cy);
                        self.screen.insert(self.scroll_bottom, vec![Cell::default(); self.cols]);
                    }
                }
            }
            b'S' => {
                let n = self.param(0, 1) as usize;
                for _ in 0..n {
                    self.scroll_up();
                }
            }
            b'T' => {
                let n = self.param(0, 1) as usize;
                for _ in 0..n {
                    self.scroll_down();
                }
            }

            // -- scrolling region ------------------------------------------
            b'r' => {
                let top = self.param(0, 1).max(1) as usize;
                let bottom = self.param(1, self.rows as i32) as usize;
                let top = top.min(self.rows);
                let bottom = bottom.min(self.rows);
                if top < bottom {
                    self.scroll_top = top - 1;
                    self.scroll_bottom = bottom - 1;
                } else {
                    self.scroll_top = 0;
                    self.scroll_bottom = self.rows - 1;
                }
                self.cx = 0;
                self.cy = if self.origin_mode { self.scroll_top } else { 0 };
            }

            // -- save and restore -------------------------------------------
            b's' => {
                self.saved = Some((self.cx, self.cy, self.pen));
            }
            b'u' => {
                if let Some((x, y, pen)) = self.saved {
                    self.cx = x.min(self.cols - 1);
                    self.cy = y.min(self.rows - 1);
                    self.pen = pen;
                }
            }

            // -- device status ----------------------------------------------
            b'n' => {
                match self.param(0, 0) {
                    5 => replies.push(Reply::Bytes(b"\x1b[0n".to_vec())),
                    6 => {
                        // cursor position, one based
                        let row = if self.origin_mode {
                            self.cy.saturating_sub(self.scroll_top) + 1
                        } else {
                            self.cy + 1
                        };
                        let col = self.cx + 1;
                        replies.push(Reply::Bytes(format!("\x1b[{row};{col}R").into_bytes()));
                    }
                    _ => {}
                }
            }
            b'c' => {
                // identify: report a VT102
                if private.is_none() {
                    replies.push(Reply::Bytes(b"\x1b[?6c".to_vec()));
                }
            }

            // -- select graphic rendition ------------------------------------
            b'm' => self.sgr(),

            // -- modes --------------------------------------------------------
            b'h' | b'l' => {
                let set = final_byte == b'h';
                if private == Some('?') {
                    for i in 0..self.params.len().max(1) {
                        match self.params.get(i).copied().unwrap_or(0) {
                            1 => self.application_cursor = set,
                            6 => {
                                self.origin_mode = set;
                                self.cx = 0;
                                self.cy = if set { self.scroll_top } else { 0 };
                            }
                            7 => self.wrap = set,
                            25 => self.cursor_visible = set,
                            _ => {}
                        }
                    }
                }
                // other modes (insert, linefeed/newline) are ignored
            }

            _ => {}
        }
    }

    fn sgr(&mut self) {
        if self.params.is_empty() {
            self.pen = Pen::default();
            return;
        }
        let params = self.params.clone();
        let mut i = 0;
        while i < params.len() {
            match params[i] {
                0 => self.pen = Pen::default(),
                1 => self.pen.bold = true,
                2 => self.pen.dim = true,
                3 => self.pen.italic = true,
                4 => self.pen.underline = true,
                7 => self.pen.reverse = true,
                9 => self.pen.strike = true,
                21 | 22 => {
                    self.pen.bold = false;
                    self.pen.dim = false;
                }
                23 => self.pen.italic = false,
                24 => self.pen.underline = false,
                27 => self.pen.reverse = false,
                29 => self.pen.strike = false,
                30..=37 => self.pen.fg = Color::Indexed((params[i] - 30) as u8),
                39 => self.pen.fg = Color::Default,
                40..=47 => self.pen.bg = Color::Indexed((params[i] - 40) as u8),
                49 => self.pen.bg = Color::Default,
                90..=97 => self.pen.fg = Color::Indexed((params[i] - 90 + 8) as u8),
                100..=107 => self.pen.bg = Color::Indexed((params[i] - 100 + 8) as u8),
                38 | 48 => {
                    // extended colour: 5;n (indexed) or 2;r;g;b (truecolour)
                    let is_fg = params[i] == 38;
                    if i + 1 < params.len() {
                        match params[i + 1] {
                            5 if i + 2 < params.len() => {
                                let idx = params[i + 2].clamp(0, 255) as u8;
                                if is_fg {
                                    self.pen.fg = Color::Indexed(idx);
                                } else {
                                    self.pen.bg = Color::Indexed(idx);
                                }
                                i += 2;
                            }
                            2 if i + 4 < params.len() => {
                                let (r, g, b) = (
                                    params[i + 2].clamp(0, 255) as u8,
                                    params[i + 3].clamp(0, 255) as u8,
                                    params[i + 4].clamp(0, 255) as u8,
                                );
                                if is_fg {
                                    self.pen.fg = Color::Rgb(r, g, b);
                                } else {
                                    self.pen.bg = Color::Rgb(r, g, b);
                                }
                                i += 4;
                            }
                            _ => {}
                        }
                    }
                }
                _ => {}
            }
            i += 1;
        }
    }

    // -- screen operations ---------------------------------------------------

    fn print(&mut self, b: u8, _replies: &mut Vec<Reply>) {
        // A multibyte character can be split across two reads from the socket, so the
        // partial sequence is kept on the terminal rather than decoded per call.
        let ch = match decode_utf8(&mut self.utf8_pending, b) {
            Some(c) => c,
            None => return,
        };

        if self.pending_wrap {
            self.pending_wrap = false;
            self.cx = 0;
            self.line_feed();
        }

        if self.cx >= self.cols {
            if self.wrap {
                self.cx = 0;
                self.line_feed();
            } else {
                self.cx = self.cols - 1;
            }
        }

        self.screen[self.cy][self.cx] = Cell { ch, pen: self.pen };

        if self.cx + 1 >= self.cols {
            // park in the last column; wrap when the next character arrives, so a
            // line exactly as wide as the screen does not scroll an extra time
            if self.wrap {
                self.pending_wrap = true;
            }
        } else {
            self.cx += 1;
        }
    }

    fn line_feed(&mut self) {
        if self.cy == self.scroll_bottom {
            self.scroll_up();
        } else if self.cy + 1 < self.rows {
            self.cy += 1;
        }
    }

    fn scroll_up(&mut self) {
        if self.scroll_top == 0 && self.scroll_bottom == self.rows - 1 {
            // the whole screen scrolls, so the top line becomes history
            let line = self.screen.remove(0);
            self.scrollback.push_back(line);
            while self.scrollback.len() > SCROLLBACK {
                self.scrollback.pop_front();
            }
            self.screen.insert(self.scroll_bottom, vec![Cell::default(); self.cols]);
        } else {
            self.screen.remove(self.scroll_top);
            self.screen
                .insert(self.scroll_bottom, vec![Cell::default(); self.cols]);
        }
    }

    fn scroll_down(&mut self) {
        self.screen.remove(self.scroll_bottom);
        self.screen
            .insert(self.scroll_top, vec![Cell::default(); self.cols]);
    }

    fn erase_cells(&mut self, row: usize, from: usize, to: usize) {
        if row >= self.rows {
            return;
        }
        let line = &mut self.screen[row];
        for x in from..to.min(self.cols) {
            // erasing with a coloured background paints it, as xterm does
            line[x] = Cell {
                ch: ' ',
                pen: Pen {
                    bg: self.pen.bg,
                    ..Pen::default()
                },
            };
        }
    }
}

/// A character being assembled across reads.
#[derive(Debug, Clone, Copy, Default)]
pub struct Utf8Pending {
    buf: [u8; 4],
    len: usize,
    need: usize,
}

/// Assemble a UTF-8 character one byte at a time.
///
/// Returns the character once complete, and `None` while more bytes are needed.
pub fn decode_utf8(pending: &mut Utf8Pending, b: u8) -> Option<char> {
    if b < 0x80 {
        pending.len = 0;
        pending.need = 0;
        return Some(b as char);
    }

    if b & 0xc0 == 0x80 {
        // continuation
        if pending.need == 0 || pending.len >= pending.need + 1 {
            pending.len = 0;
            pending.need = 0;
            return Some('\u{fffd}');
        }
        pending.buf[pending.len] = b;
        pending.len += 1;
        if pending.len == pending.need + 1 {
            let slice = &pending.buf[..pending.len];
            let s = std::str::from_utf8(slice).ok();
            pending.len = 0;
            pending.need = 0;
            return Some(s.and_then(|s| s.chars().next()).unwrap_or('\u{fffd}'));
        }
        return None;
    }

    // a leading byte
    let need = if b & 0xe0 == 0xc0 {
        1
    } else if b & 0xf0 == 0xe0 {
        2
    } else if b & 0xf8 == 0xf0 {
        3
    } else {
        return Some('\u{fffd}');
    };
    pending.buf[0] = b;
    pending.len = 1;
    pending.need = need;
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text(t: &Terminal) -> String {
        t.dump()
    }

    #[test]
    fn plain_text_and_newlines() {
        let mut t = Terminal::new(4, 10);
        t.feed(b"hi\r\nthere\r\n");
        let d = text(&t);
        assert!(d.starts_with("hi\nthere\n"), "got {d:?}");
    }

    #[test]
    fn carriage_return_overwrites() {
        let mut t = Terminal::new(3, 10);
        t.feed(b"abcdef\rXY");
        assert!(text(&t).starts_with("XYcdef"), "got {:?}", text(&t));
    }

    #[test]
    fn backspace_moves_back() {
        let mut t = Terminal::new(3, 10);
        t.feed(b"abc\x08X");
        assert!(text(&t).starts_with("abX"), "got {:?}", text(&t));
    }

    #[test]
    fn cursor_position_and_write() {
        let mut t = Terminal::new(3, 10);
        t.feed(b"\x1b[2;3HZ");
        let dumped = t.dump();
        let lines: Vec<&str> = dumped.lines().collect();
        assert_eq!(lines[1].trim_end(), "  Z");
    }

    #[test]
    fn erase_in_line() {
        let mut t = Terminal::new(3, 10);
        t.feed(b"abcdef\x1b[3G\x1b[K");
        assert_eq!(text(&t).lines().next().unwrap().trim_end(), "ab");
    }

    #[test]
    fn erase_in_display() {
        let mut t = Terminal::new(4, 10);
        t.feed(b"one\r\ntwo\r\nthree\x1b[2J");
        assert!(text(&t).trim().is_empty(), "got {:?}", text(&t));
    }

    #[test]
    fn colours_are_recorded() {
        let mut t = Terminal::new(2, 10);
        t.feed(b"\x1b[31mA\x1b[0mB");
        let line = t.line(t.total_lines() - 2).unwrap();
        assert_eq!(line[0].pen.fg, Color::Indexed(1));
        assert_eq!(line[1].pen.fg, Color::Default);
    }

    #[test]
    fn truecolour() {
        let mut t = Terminal::new(2, 10);
        t.feed(b"\x1b[38;2;10;20;30mX");
        let line = t.line(t.total_lines() - 2).unwrap();
        assert_eq!(line[0].pen.fg, Color::Rgb(10, 20, 30));
    }

    #[test]
    fn bold_and_reset() {
        let mut t = Terminal::new(2, 10);
        t.feed(b"\x1b[1mB\x1b[22mN");
        let line = t.line(t.total_lines() - 2).unwrap();
        assert!(line[0].pen.bold);
        assert!(!line[1].pen.bold);
    }

    #[test]
    fn scrolls_and_keeps_history() {
        let mut t = Terminal::new(2, 10);
        t.feed(b"one\r\ntwo\r\nthree\r\nfour\r\n");
        assert_eq!(t.rows(), 2);
        // four lines written into a two line screen: the first two are history
        let all: Vec<String> = (0..t.total_lines())
            .filter_map(|i| t.line(i))
            .map(|l| l.iter().map(|c| c.ch).collect::<String>().trim_end().to_string())
            .collect();
        assert!(all.len() >= 4, "got {all:?}");
        assert_eq!(all[0], "one");
        assert_eq!(all[1], "two");
    }

    #[test]
    fn wrapping_at_the_right_margin() {
        let mut t = Terminal::new(3, 5);
        t.feed(b"abcdefgh");
        let d = t.dump();
        assert!(d.starts_with("abcde\nfgh"), "got {d:?}");
    }

    #[test]
    fn line_exactly_as_wide_does_not_double_scroll() {
        let mut t = Terminal::new(2, 5);
        t.feed(b"abcde");
        // still on the first line, parked in the last column
        assert_eq!(t.cursor().1, 0);
    }

    #[test]
    fn device_status_report_answers() {
        let mut t = Terminal::new(5, 20);
        t.feed(b"\x1b[3;4H");
        let replies = t.feed(b"\x1b[6n");
        assert_eq!(replies, vec![Reply::Bytes(b"\x1b[3;4R".to_vec())]);
    }

    #[test]
    fn insert_and_delete_lines() {
        let mut t = Terminal::new(4, 6);
        t.feed(b"one\r\ntwo\r\nthree");
        t.feed(b"\x1b[1;1H\x1b[L");
        let d = t.dump();
        let lines: Vec<&str> = d.lines().map(|s| s.trim_end()).collect();
        assert_eq!(lines[0], "");
        assert_eq!(lines[1], "one");
    }

    #[test]
    fn scrolling_region_confines_the_scroll() {
        let mut t = Terminal::new(4, 6);
        t.feed(b"a\r\nb\r\nc\r\nd");
        // region is rows 2..3 (one based), so only those scroll
        t.feed(b"\x1b[2;3r\x1b[3;1H\n");
        let lines: Vec<String> = t
            .dump()
            .lines()
            .map(|s| s.trim_end().to_string())
            .collect();
        assert_eq!(lines[0], "a");
    }

    #[test]
    fn multibyte_characters() {
        let mut t = Terminal::new(2, 10);
        t.feed("中".as_bytes());
        let line = t.line(t.total_lines() - 2).unwrap();
        assert_eq!(line[0].ch, '中');
    }

    #[test]
    fn resize_keeps_content() {
        let mut t = Terminal::new(4, 10);
        t.feed(b"hello\r\nworld");
        t.resize(6, 20);
        assert_eq!(t.rows(), 6);
        assert_eq!(t.cols(), 20);
        let d = t.dump();
        assert!(d.contains("hello"), "got {d:?}");
        assert!(d.contains("world"), "got {d:?}");
    }

    #[test]
    fn osc_title_is_swallowed() {
        let mut t = Terminal::new(2, 10);
        t.feed(b"\x1b]0;my title\x07ok");
        assert!(text(&t).starts_with("ok"), "got {:?}", text(&t));
    }

    #[test]
    fn cursor_hide_and_show() {
        let mut t = Terminal::new(2, 10);
        assert!(t.cursor_visible());
        t.feed(b"\x1b[?25l");
        assert!(!t.cursor_visible());
        t.feed(b"\x1b[?25h");
        assert!(t.cursor_visible());
    }
}
