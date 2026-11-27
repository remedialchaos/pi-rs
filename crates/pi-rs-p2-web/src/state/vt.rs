//! Pure-Rust ANSI/VT100 terminal screen buffer and key-to-VT byte encoder
//! (Wave 1.6b PTY terminal support).

use crate::markdown::escape_html;

/// ANSI/VT cell colors.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum VtColor {
    #[default]
    Default,
    Indexed(u8),
    Rgb(u8, u8, u8),
}

/// Text attributes for a VT cell.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct VtStyle {
    pub fg: VtColor,
    pub bg: VtColor,
    pub bold: bool,
    pub dim: bool,
    pub italic: bool,
    pub underline: bool,
    pub inverse: bool,
}

/// One cell in the terminal grid.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct VtCell {
    pub ch: char,
    pub style: VtStyle,
}

impl Default for VtCell {
    fn default() -> Self {
        Self {
            ch: ' ',
            style: VtStyle::default(),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum ParserState {
    Ground,
    Escape,
    Csi(String),
    Osc(String),
    OscEsc(String),
}

/// Incremental ANSI/VT screen buffer with scrollback, cursor addressing, SGR styles, and OSC titles.
#[derive(Clone, Debug)]
pub struct VtScreen {
    cols: usize,
    rows: usize,
    cursor_row: usize,
    cursor_col: usize,
    saved_cursor: (usize, usize),
    grid: Vec<Vec<VtCell>>,
    scrollback: Vec<Vec<VtCell>>,
    style: VtStyle,
    title: Option<String>,
    state: ParserState,
    utf8_buf: Vec<u8>,
}

impl VtScreen {
    pub fn new(cols: u16, rows: u16) -> Self {
        let c = usize::from(cols.max(1));
        let r = usize::from(rows.max(1));
        Self {
            cols: c,
            rows: r,
            cursor_row: 0,
            cursor_col: 0,
            saved_cursor: (0, 0),
            grid: vec![vec![VtCell::default(); c]; r],
            scrollback: Vec::new(),
            style: VtStyle::default(),
            title: None,
            state: ParserState::Ground,
            utf8_buf: Vec::new(),
        }
    }

    pub fn cols(&self) -> usize {
        self.cols
    }

    pub fn rows(&self) -> usize {
        self.rows
    }

    pub fn cursor(&self) -> (usize, usize) {
        (self.cursor_row, self.cursor_col)
    }

    pub fn title(&self) -> Option<&str> {
        self.title.as_deref()
    }

    pub fn resize(&mut self, cols: u16, rows: u16) {
        let new_cols = usize::from(cols.max(1));
        let new_rows = usize::from(rows.max(1));
        for row in &mut self.grid {
            row.resize(new_cols, VtCell::default());
        }
        while self.grid.len() < new_rows {
            self.grid.push(vec![VtCell::default(); new_cols]);
        }
        if self.grid.len() > new_rows {
            let extra = self.grid.len() - new_rows;
            self.scrollback.extend(self.grid.drain(..extra));
            if self.scrollback.len() > 2000 {
                let drop_n = self.scrollback.len() - 2000;
                self.scrollback.drain(..drop_n);
            }
        }
        self.cols = new_cols;
        self.rows = new_rows;
        self.cursor_row = self.cursor_row.min(new_rows - 1);
        self.cursor_col = self.cursor_col.min(new_cols - 1);
    }

    pub fn feed(&mut self, bytes: &[u8]) {
        for &b in bytes {
            self.feed_byte(b);
        }
    }

    fn feed_byte(&mut self, b: u8) {
        if !matches!(self.state, ParserState::Ground) {
            self.utf8_buf.clear();
            let state = std::mem::replace(&mut self.state, ParserState::Ground);
            match state {
                ParserState::Escape => match b {
                    b'[' => self.state = ParserState::Csi(String::new()),
                    b']' => self.state = ParserState::Osc(String::new()),
                    b'7' => self.saved_cursor = (self.cursor_row, self.cursor_col),
                    b'8' => {
                        self.cursor_row = self.saved_cursor.0.min(self.rows - 1);
                        self.cursor_col = self.saved_cursor.1.min(self.cols - 1);
                    }
                    b'c' => {
                        let (c, r) = (self.cols as u16, self.rows as u16);
                        *self = Self::new(c, r);
                    }
                    _ => {}
                },
                ParserState::Csi(mut buf) => {
                    if (0x40..=0x7E).contains(&b) {
                        self.exec_csi(&buf, b as char);
                    } else if buf.len() < 128 {
                        buf.push(b as char);
                        self.state = ParserState::Csi(buf);
                    }
                }
                ParserState::Osc(mut buf) => {
                    if b == 0x07 {
                        self.exec_osc(&buf);
                    } else if b == 0x1b {
                        self.state = ParserState::OscEsc(buf);
                    } else if buf.len() < 512 {
                        buf.push(b as char);
                        self.state = ParserState::Osc(buf);
                    }
                }
                ParserState::OscEsc(buf) => {
                    if b == b'\\' {
                        self.exec_osc(&buf);
                    } else {
                        self.state = ParserState::Ground;
                    }
                }
                ParserState::Ground => {}
            }
            return;
        }

        match b {
            0x1b => {
                self.utf8_buf.clear();
                self.state = ParserState::Escape;
            }
            b'\r' => {
                self.utf8_buf.clear();
                self.cursor_col = 0;
            }
            b'\n' => {
                self.utf8_buf.clear();
                self.line_feed();
            }
            0x08 => {
                self.utf8_buf.clear();
                self.cursor_col = self.cursor_col.saturating_sub(1);
            }
            b'\t' => {
                self.utf8_buf.clear();
                let next_tab = ((self.cursor_col / 8) + 1) * 8;
                self.cursor_col = next_tab.min(self.cols - 1);
            }
            0x00..=0x1f | 0x7f => {
                self.utf8_buf.clear();
            }
            0x20..=0x7e => {
                self.utf8_buf.clear();
                self.put_char(b as char);
            }
            _ => {
                self.utf8_buf.push(b);
                if let Ok(s) = std::str::from_utf8(&self.utf8_buf) {
                    let chars: Vec<char> = s.chars().collect();
                    self.utf8_buf.clear();
                    for ch in chars {
                        self.put_char(ch);
                    }
                } else if self.utf8_buf.len() >= 4 {
                    self.utf8_buf.clear();
                }
            }
        }
    }

    fn put_char(&mut self, ch: char) {
        if self.cursor_col >= self.cols {
            self.cursor_col = 0;
            self.line_feed();
        }
        if let Some(row) = self.grid.get_mut(self.cursor_row)
            && let Some(cell) = row.get_mut(self.cursor_col)
        {
            *cell = VtCell {
                ch,
                style: self.style,
            };
        }
        self.cursor_col += 1;
    }

    fn line_feed(&mut self) {
        if self.cursor_row + 1 < self.rows {
            self.cursor_row += 1;
        } else {
            let evicted = self.grid.remove(0);
            self.scrollback.push(evicted);
            if self.scrollback.len() > 2000 {
                self.scrollback.remove(0);
            }
            self.grid.push(vec![VtCell::default(); self.cols]);
        }
    }

    fn exec_osc(&mut self, buf: &str) {
        if let Some(rest) = buf.strip_prefix("0;").or_else(|| buf.strip_prefix("2;")) {
            self.title = Some(rest.to_owned());
        }
    }

    fn exec_csi(&mut self, raw: &str, final_ch: char) {
        let cleaned = raw.trim_start_matches(['?', '>', '!']);
        let params: Vec<usize> = if cleaned.is_empty() {
            Vec::new()
        } else {
            cleaned
                .split(';')
                .map(|p| p.parse::<usize>().unwrap_or(0))
                .collect()
        };
        let p = |idx: usize, default: usize| -> usize {
            params
                .get(idx)
                .copied()
                .filter(|&v| v > 0)
                .unwrap_or(default)
        };
        match final_ch {
            'A' => self.cursor_row = self.cursor_row.saturating_sub(p(0, 1)),
            'B' => self.cursor_row = (self.cursor_row + p(0, 1)).min(self.rows - 1),
            'C' => self.cursor_col = (self.cursor_col + p(0, 1)).min(self.cols - 1),
            'D' => self.cursor_col = self.cursor_col.saturating_sub(p(0, 1)),
            'G' => self.cursor_col = p(0, 1).saturating_sub(1).min(self.cols - 1),
            'H' | 'f' => {
                self.cursor_row = p(0, 1).saturating_sub(1).min(self.rows - 1);
                self.cursor_col = p(1, 1).saturating_sub(1).min(self.cols - 1);
            }
            'J' => {
                let mode = params.first().copied().unwrap_or(0);
                match mode {
                    0 => {
                        self.clear_range(self.cursor_row, self.cursor_col, self.cols);
                        for r in (self.cursor_row + 1)..self.rows {
                            self.clear_range(r, 0, self.cols);
                        }
                    }
                    1 => {
                        for r in 0..self.cursor_row {
                            self.clear_range(r, 0, self.cols);
                        }
                        self.clear_range(self.cursor_row, 0, (self.cursor_col + 1).min(self.cols));
                    }
                    2 | 3 => {
                        for r in 0..self.rows {
                            self.clear_range(r, 0, self.cols);
                        }
                        if mode == 3 {
                            self.scrollback.clear();
                        }
                    }
                    _ => {}
                }
            }
            'K' => {
                let mode = params.first().copied().unwrap_or(0);
                match mode {
                    0 => self.clear_range(self.cursor_row, self.cursor_col, self.cols),
                    1 => self.clear_range(self.cursor_row, 0, (self.cursor_col + 1).min(self.cols)),
                    2 => self.clear_range(self.cursor_row, 0, self.cols),
                    _ => {}
                }
            }
            's' => self.saved_cursor = (self.cursor_row, self.cursor_col),
            'u' => {
                self.cursor_row = self.saved_cursor.0.min(self.rows - 1);
                self.cursor_col = self.saved_cursor.1.min(self.cols - 1);
            }
            'm' => self.apply_sgr(&params),
            _ => {}
        }
    }

    fn clear_range(&mut self, row: usize, start: usize, end: usize) {
        if let Some(line) = self.grid.get_mut(row) {
            for cell in line.iter_mut().take(end.min(self.cols)).skip(start) {
                *cell = VtCell {
                    ch: ' ',
                    style: self.style,
                };
            }
        }
    }

    fn apply_sgr(&mut self, params: &[usize]) {
        if params.is_empty() {
            self.style = VtStyle::default();
            return;
        }
        let mut i = 0;
        while i < params.len() {
            match params[i] {
                0 => self.style = VtStyle::default(),
                1 => self.style.bold = true,
                2 => self.style.dim = true,
                3 => self.style.italic = true,
                4 => self.style.underline = true,
                7 => self.style.inverse = true,
                22 => {
                    self.style.bold = false;
                    self.style.dim = false;
                }
                23 => self.style.italic = false,
                24 => self.style.underline = false,
                27 => self.style.inverse = false,
                30..=37 => self.style.fg = VtColor::Indexed((params[i] - 30) as u8),
                39 => self.style.fg = VtColor::Default,
                40..=47 => self.style.bg = VtColor::Indexed((params[i] - 40) as u8),
                49 => self.style.bg = VtColor::Default,
                90..=97 => self.style.fg = VtColor::Indexed((params[i] - 90 + 8) as u8),
                100..=107 => self.style.bg = VtColor::Indexed((params[i] - 100 + 8) as u8),
                38 | 48 => {
                    let is_fg = params[i] == 38;
                    if params.get(i + 1) == Some(&5)
                        && let Some(&idx) = params.get(i + 2)
                    {
                        let color = VtColor::Indexed(idx.min(255) as u8);
                        if is_fg {
                            self.style.fg = color;
                        } else {
                            self.style.bg = color;
                        }
                        i += 2;
                    } else if params.get(i + 1) == Some(&2)
                        && let (Some(&r), Some(&g), Some(&b)) =
                            (params.get(i + 2), params.get(i + 3), params.get(i + 4))
                    {
                        let color =
                            VtColor::Rgb(r.min(255) as u8, g.min(255) as u8, b.min(255) as u8);
                        if is_fg {
                            self.style.fg = color;
                        } else {
                            self.style.bg = color;
                        }
                        i += 4;
                    }
                }
                _ => {}
            }
            i += 1;
        }
    }

    /// Plain text lines (scrollback + active rows), trimming trailing empty rows.
    pub fn plain_lines(&self) -> Vec<String> {
        let mut lines: Vec<String> = self
            .scrollback
            .iter()
            .chain(self.grid.iter())
            .map(|row| {
                let s: String = row.iter().map(|c| c.ch).collect();
                s.trim_end().to_owned()
            })
            .collect();
        while lines.last().is_some_and(String::is_empty) && lines.len() > 1 {
            lines.pop();
        }
        lines
    }

    /// HTML-escaped styled lines for live terminal rendering.
    pub fn styled_lines_html(&self) -> Vec<String> {
        let total = self.scrollback.len() + self.grid.len();
        let active_offset = self.scrollback.len();
        let mut out = Vec::with_capacity(total);
        for (idx, row) in self.scrollback.iter().chain(self.grid.iter()).enumerate() {
            let cursor_here = idx >= active_offset && (idx - active_offset) == self.cursor_row;
            let mut trimmed_len = row
                .iter()
                .rposition(|c| c.ch != ' ' || c.style != VtStyle::default())
                .map_or(0, |p| p + 1);
            if cursor_here {
                trimmed_len = trimmed_len.max((self.cursor_col + 1).min(row.len()));
            }
            if trimmed_len == 0 {
                out.push(String::new());
                continue;
            }
            let mut html = String::new();
            for (col, cell) in row[..trimmed_len].iter().enumerate() {
                let is_cursor = cursor_here && col == self.cursor_col;
                let css = cell_css(&cell.style, is_cursor);
                let ch = escape_html(&cell.ch.to_string());
                if css.is_empty() {
                    html.push_str(&ch);
                } else {
                    html.push_str("<span style=\"");
                    html.push_str(&css);
                    html.push_str("\">");
                    html.push_str(&ch);
                    html.push_str("</span>");
                }
            }
            out.push(html);
        }
        while out.last().is_some_and(String::is_empty) && out.len() > 1 {
            out.pop();
        }
        out
    }
}

fn ansi_hex(idx: u8) -> &'static str {
    match idx {
        0 => "#1d1b19",
        1 => "#c8433d",
        2 => "#2f9e6a",
        3 => "#c98b2c",
        4 => "#2f7de1",
        5 => "#a45bd6",
        6 => "#2a9d9d",
        7 => "#d8d4ce",
        8 => "#77716a",
        9 => "#f07a72",
        10 => "#5cc493",
        11 => "#f5c451",
        12 => "#5b9cf0",
        13 => "#c88df0",
        14 => "#56cfcf",
        15 => "#fbfaf8",
        _ => "#9b958d",
    }
}

fn color_css(c: VtColor) -> Option<String> {
    match c {
        VtColor::Default => None,
        VtColor::Indexed(i) => Some(ansi_hex(i).to_owned()),
        VtColor::Rgb(r, g, b) => Some(format!("rgb({r},{g},{b})")),
    }
}

fn cell_css(style: &VtStyle, is_cursor: bool) -> String {
    let mut parts = Vec::new();
    let (fg, bg) = if style.inverse || is_cursor {
        (
            color_css(style.bg).unwrap_or_else(|| "var(--bg)".to_owned()),
            color_css(style.fg).unwrap_or_else(|| "var(--text)".to_owned()),
        )
    } else {
        (
            color_css(style.fg).unwrap_or_default(),
            color_css(style.bg).unwrap_or_default(),
        )
    };
    if !fg.is_empty() {
        parts.push(format!("color:{fg}"));
    }
    if !bg.is_empty() {
        parts.push(format!("background:{bg}"));
    }
    if style.bold {
        parts.push("font-weight:700".to_owned());
    }
    if style.dim {
        parts.push("opacity:0.7".to_owned());
    }
    if style.italic {
        parts.push("font-style:italic".to_owned());
    }
    if style.underline {
        parts.push("text-decoration:underline".to_owned());
    }
    parts.join(";")
}

/// Encodes a DOM keyboard event into raw PTY input bytes.
pub fn encode_terminal_key(key: &str, ctrl: bool, alt: bool, _shift: bool) -> Option<Vec<u8>> {
    if ctrl {
        let mut chars = key.chars();
        if let (Some(ch), None) = (chars.next(), chars.next()) {
            let lower = ch.to_ascii_lowercase();
            if lower.is_ascii_lowercase() {
                return Some(vec![(lower as u8) - b'a' + 1]);
            }
        }
    }
    let seq: &[u8] = match key {
        "Enter" => b"\r",
        "Backspace" => b"\x7f",
        "Tab" => b"\t",
        "Escape" => b"\x1b",
        "ArrowUp" => b"\x1b[A",
        "ArrowDown" => b"\x1b[B",
        "ArrowRight" => b"\x1b[C",
        "ArrowLeft" => b"\x1b[D",
        "Home" => b"\x1b[H",
        "End" => b"\x1b[F",
        "PageUp" => b"\x1b[5~",
        "PageDown" => b"\x1b[6~",
        "Delete" => b"\x1b[3~",
        other if other.chars().count() == 1 => {
            let mut out = Vec::new();
            if alt {
                out.push(0x1b);
            }
            out.extend_from_slice(other.as_bytes());
            return Some(out);
        }
        _ => return None,
    };
    let mut out = Vec::new();
    if alt && !seq.starts_with(b"\x1b") {
        out.push(0x1b);
    }
    out.extend_from_slice(seq);
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn vt_screen_handles_csi_cursor_sgr_and_osc_title() {
        let mut vt = VtScreen::new(40, 10);
        vt.feed(b"\x1b]0;bash-pty\x07\x1b[32mhello\x1b[0m\r\nworld");
        assert_eq!(vt.title(), Some("bash-pty"));
        let lines = vt.plain_lines();
        assert_eq!(lines[0], "hello");
        assert_eq!(lines[1], "world");
        let styled = vt.styled_lines_html();
        assert!(styled[0].contains("#2f9e6a"));

        vt.feed(b"\x1b[1;1H\x1b[2Kreplaced");
        assert_eq!(vt.plain_lines()[0], "replaced");
    }

    #[test]
    fn key_encoder_maps_control_and_arrows() {
        assert_eq!(
            encode_terminal_key("c", true, false, false),
            Some(vec![0x03])
        );
        assert_eq!(
            encode_terminal_key("ArrowUp", false, false, false),
            Some(b"\x1b[A".to_vec())
        );
        assert_eq!(
            encode_terminal_key("Enter", false, false, false),
            Some(b"\r".to_vec())
        );
        assert_eq!(encode_terminal_key("Shift", false, false, true), None);
    }
}
