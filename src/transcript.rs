use chrono::{DateTime, Duration, FixedOffset, SecondsFormat};
use unicode_width::UnicodeWidthChar;
use vte::{Params, Perform};

pub const VERSION: u32 = 1;
#[derive(Clone)]
struct Row {
    cells: Vec<String>,
    dirty: bool,
    time: u64,
    continued: bool,
}
impl Row {
    fn new(cols: usize) -> Self {
        Self {
            cells: vec![" ".into(); cols],
            dirty: false,
            time: 0,
            continued: false,
        }
    }
    fn text(&self) -> String {
        self.cells.concat().trim_end_matches(' ').to_owned()
    }
}
pub struct Transcript {
    parser: vte::Parser,
    screen: Screen,
}
struct Screen {
    rows: Vec<Row>,
    cols: usize,
    row: usize,
    col: usize,
    wrap: bool,
    alt: bool,
    saved: (usize, usize),
    time: u64,
    start: DateTime<FixedOffset>,
    lines: Vec<String>,
    prefix: String,
    prefix_time: u64,
    prefix_dirty: bool,
    scroll_top: usize,
    scroll_bottom: usize,
}
impl Transcript {
    pub fn new(rows: u16, cols: u16, start: DateTime<FixedOffset>) -> Self {
        let rows = usize::from(rows.clamp(1, 1000));
        let cols = usize::from(cols.clamp(1, 2000));
        Self {
            parser: vte::Parser::new(),
            screen: Screen {
                rows: vec![Row::new(cols); rows],
                cols,
                row: 0,
                col: 0,
                wrap: false,
                alt: false,
                saved: (0, 0),
                time: 0,
                start,
                lines: vec![],
                prefix: String::new(),
                prefix_time: 0,
                prefix_dirty: false,
                scroll_top: 0,
                scroll_bottom: rows - 1,
            },
        }
    }
    pub fn feed(&mut self, text: &str, micros: u64) -> Vec<String> {
        self.screen.time = micros;
        self.parser.advance(&mut self.screen, text.as_bytes());
        self.take()
    }
    pub fn resize(&mut self, rows: u16, cols: u16) -> Vec<String> {
        // Preserve pending text before changing the coordinate system.
        self.screen.flush();
        let h = usize::from(rows.clamp(1, 1000));
        let w = usize::from(cols.clamp(1, 2000));
        self.screen.cols = w;
        self.screen.rows = vec![Row::new(w); h];
        self.screen.row = 0;
        self.screen.col = 0;
        self.screen.wrap = false;
        self.screen.scroll_top = 0;
        self.screen.scroll_bottom = h - 1;
        self.take()
    }
    pub fn finish(&mut self) -> Vec<String> {
        self.screen.flush();
        self.take()
    }
    fn take(&mut self) -> Vec<String> {
        std::mem::take(&mut self.screen.lines)
    }
}
impl Screen {
    fn emit(&mut self, text: String, time: u64) {
        let t = self.start + Duration::microseconds(time.min(i64::MAX as u64) as i64);
        self.lines.push(format!(
            "{}\t{}\n",
            t.to_rfc3339_opts(SecondsFormat::Micros, false),
            text
        ));
    }
    fn commit(&mut self, end: usize) {
        if self.alt {
            return;
        }
        let mut begin = end;
        while begin > 0 && self.rows[begin].continued {
            begin -= 1;
        }
        let mut text = if begin == 0 {
            self.prefix.clone()
        } else {
            String::new()
        };
        let mut time = if begin == 0 { self.prefix_time } else { 0 };
        let mut dirty = begin == 0 && self.prefix_dirty;
        for i in begin..=end {
            // Preserve spaces at a soft wrap, trim only the logical end.
            text.push_str(&if i < end {
                self.rows[i].cells.concat()
            } else {
                self.rows[i].text()
            });
            time = time.max(self.rows[i].time);
            dirty |= self.rows[i].dirty;
            self.rows[i].dirty = false;
        }
        if dirty {
            self.emit(text, time);
        }
        if begin == 0 {
            self.prefix.clear();
            self.prefix_dirty = false;
            self.prefix_time = 0;
        }
    }
    fn flush(&mut self) {
        if self.alt {
            return;
        }
        for i in 0..self.rows.len() {
            if i + 1 == self.rows.len() || !self.rows[i + 1].continued {
                self.commit(i);
            }
        }
    }
    fn advance_row(&mut self, continued: bool) {
        self.wrap = false;
        if self.row == self.scroll_bottom {
            let old = self.rows.remove(self.scroll_top);
            if !self.alt && self.scroll_top == 0 {
                if self.rows.first().is_some_and(|r| r.continued) {
                    self.prefix.push_str(&old.cells.concat());
                    self.prefix_time = self.prefix_time.max(old.time);
                    self.prefix_dirty |= old.dirty;
                } else {
                    if old.dirty || self.prefix_dirty {
                        let text = format!("{}{}", self.prefix, old.text());
                        self.emit(text, old.time.max(self.prefix_time));
                    }
                    self.prefix.clear();
                    self.prefix_dirty = false;
                    self.prefix_time = 0;
                }
            }
            self.rows.insert(self.scroll_bottom, Row::new(self.cols));
        } else {
            self.row = (self.row + 1).min(self.rows.len() - 1);
        }
        self.rows[self.row].continued = continued;
    }
    fn erase(&mut self, row: usize, from: usize, to: usize) {
        for i in from..to.min(self.cols) {
            self.rows[row].cells[i] = " ".into();
        }
        self.rows[row].dirty = true;
        self.rows[row].time = self.time;
    }
}
impl Perform for Screen {
    fn print(&mut self, c: char) {
        if self.alt {
            return;
        }
        let width = c.width().unwrap_or(0).min(2);
        if width == 0 {
            let mut x = if self.wrap {
                self.col
            } else {
                self.col.saturating_sub(1)
            };
            if self.rows[self.row].cells[x].is_empty() {
                x = x.saturating_sub(1);
            }
            self.rows[self.row].cells[x].push(c);
            self.rows[self.row].dirty = true;
            self.rows[self.row].time = self.time;
            return;
        }
        if self.wrap || self.col + width > self.cols {
            self.col = 0;
            self.advance_row(true);
        }
        let r = &mut self.rows[self.row];
        if r.cells[self.col].is_empty() && self.col > 0 {
            r.cells[self.col - 1] = " ".into();
        }
        if self.col + 1 < self.cols && r.cells[self.col + 1].is_empty() {
            r.cells[self.col + 1] = " ".into();
        }
        r.cells[self.col] = c.to_string();
        if width == 2 && self.col + 1 < self.cols {
            r.cells[self.col + 1].clear();
        }
        r.dirty = true;
        r.time = self.time;
        if self.col + width >= self.cols {
            self.col = self.cols - 1;
            self.wrap = true;
        } else {
            self.col += width;
        }
    }
    fn execute(&mut self, b: u8) {
        if self.alt {
            return;
        }
        match b {
            b'\r' => {
                self.col = 0;
                self.wrap = false;
            }
            b'\n' | 0x0b | 0x0c => {
                if !self.rows[self.row].dirty
                    && self.rows[self.row].text().is_empty()
                    && !self.rows[self.row].continued
                {
                    self.rows[self.row].dirty = true;
                    self.rows[self.row].time = self.time;
                }
                self.commit(self.row);
                self.advance_row(false);
            }
            8 => {
                self.col = self.col.saturating_sub(1);
                self.wrap = false;
            }
            b'\t' => {
                self.col = ((self.col / 8 + 1) * 8).min(self.cols - 1);
                self.wrap = false;
            }
            _ => (),
        }
    }
    fn csi_dispatch(&mut self, params: &Params, intermediates: &[u8], ignore: bool, action: char) {
        if ignore {
            return;
        }
        let values: Vec<usize> = params.iter().map(|x| x[0] as usize).collect();
        let p = |i: usize, default: usize| {
            values
                .get(i)
                .copied()
                .filter(|v| *v != 0)
                .unwrap_or(default)
        };
        if intermediates == b"?" && matches!(action, 'h' | 'l') {
            if values.iter().any(|v| matches!(v, 47 | 1047 | 1049)) {
                let entering = action == 'h';
                if entering && !self.alt {
                    self.flush();
                    self.emit("[termlog: alternate screen entered]".into(), self.time);
                    self.alt = true;
                } else if !entering && self.alt {
                    self.alt = false;
                    self.emit("[termlog: alternate screen exited]".into(), self.time);
                }
            }
            return;
        }
        if self.alt || !intermediates.is_empty() {
            return;
        }
        let n = p(0, 1);
        match action {
            'A' => self.row = self.row.saturating_sub(n),
            'B' | 'e' => self.row = (self.row + n).min(self.rows.len() - 1),
            'C' | 'a' => self.col = (self.col + n).min(self.cols - 1),
            'D' => self.col = self.col.saturating_sub(n),
            'E' => {
                self.row = (self.row + n).min(self.rows.len() - 1);
                self.col = 0;
            }
            'F' => {
                self.row = self.row.saturating_sub(n);
                self.col = 0;
            }
            'G' | '`' => self.col = n.saturating_sub(1).min(self.cols - 1),
            'd' => self.row = n.saturating_sub(1).min(self.rows.len() - 1),
            'H' | 'f' => {
                self.row = n.saturating_sub(1).min(self.rows.len() - 1);
                self.col = p(1, 1).saturating_sub(1).min(self.cols - 1);
            }
            'K' => match values.first().copied().unwrap_or(0) {
                0 => self.erase(self.row, self.col, self.cols),
                1 => self.erase(self.row, 0, self.col + 1),
                2 => self.erase(self.row, 0, self.cols),
                _ => (),
            },
            'J' => match values.first().copied().unwrap_or(0) {
                0 => {
                    self.erase(self.row, self.col, self.cols);
                    for i in self.row + 1..self.rows.len() {
                        self.erase(i, 0, self.cols);
                    }
                }
                1 => {
                    for i in 0..self.row {
                        self.erase(i, 0, self.cols);
                    }
                    self.erase(self.row, 0, self.col + 1);
                }
                2 | 3 => {
                    self.flush();
                    for r in &mut self.rows {
                        *r = Row::new(self.cols);
                    }
                }
                _ => (),
            },
            'P' => {
                let row = &mut self.rows[self.row];
                for _ in 0..n.min(self.cols - self.col) {
                    row.cells.remove(self.col);
                    row.cells.push(" ".into());
                }
                row.dirty = true;
                row.time = self.time;
            }
            '@' => {
                let row = &mut self.rows[self.row];
                for _ in 0..n.min(self.cols - self.col) {
                    row.cells.insert(self.col, " ".into());
                    row.cells.pop();
                }
                row.dirty = true;
                row.time = self.time;
            }
            'X' => self.erase(self.row, self.col, self.col + n),
            's' => self.saved = (self.row, self.col),
            'u' => {
                self.row = self.saved.0.min(self.rows.len() - 1);
                self.col = self.saved.1.min(self.cols - 1);
            }
            'r' => {
                let top = n.saturating_sub(1);
                let bottom = p(1, self.rows.len()).saturating_sub(1);
                if top < bottom && bottom < self.rows.len() {
                    self.scroll_top = top;
                    self.scroll_bottom = bottom;
                    self.row = 0;
                    self.col = 0;
                }
            }
            _ => return,
        }
        self.wrap = false;
    }
    fn esc_dispatch(&mut self, _: &[u8], ignore: bool, byte: u8) {
        if ignore || self.alt {
            return;
        }
        match byte {
            b'7' => self.saved = (self.row, self.col),
            b'8' => {
                self.row = self.saved.0.min(self.rows.len() - 1);
                self.col = self.saved.1.min(self.cols - 1);
            }
            b'D' => self.advance_row(false),
            b'E' => {
                self.commit(self.row);
                self.col = 0;
                self.advance_row(false);
            }
            _ => (),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn render(text: &str, cols: u16) -> String {
        let mut parser = Transcript::new(
            3,
            cols,
            DateTime::parse_from_rfc3339("2026-09-20T00:00:00Z").unwrap(),
        );
        let mut lines = parser.feed(text, 1200);
        lines.extend(parser.finish());
        lines
            .into_iter()
            .map(|line| line.split_once('\t').unwrap().1.to_owned())
            .collect()
    }
    #[test]
    fn normalize_terminal_edits() {
        assert_eq!(render("long progress\rshort\x1b[K\r\n\x1b[31mfoX\x08o\x1b[0m\x1b]0;title\x07\r\n\r\nabc\x1b[2DX",80), "short\nfoo\n\naXc\n");
    }
    #[test]
    fn join_unicode_wraps_without_repeating_scrolled_lines() {
        assert_eq!(
            render("日本語abcdefghij\r\none\r\ntwo\r\nthree\r\nfour\r\n", 4),
            "日本語abcdefghij\none\ntwo\nthree\nfour\n"
        );
    }
    #[test]
    fn omit_alternate_screen() {
        assert_eq!(render("before\r\n\x1b[?1049hSECRET\x1b[?1049lafter",80), "before\n[termlog: alternate screen entered]\n[termlog: alternate screen exited]\nafter\n");
    }
    #[test]
    fn flush_prompt_with_last_update_time() {
        let mut parser = Transcript::new(
            3,
            80,
            DateTime::parse_from_rfc3339("2026-09-20T00:00:00Z").unwrap(),
        );
        assert!(parser.feed("pro", 1).is_empty());
        assert!(parser.feed("mpt", 2345).is_empty());
        assert_eq!(
            parser.finish(),
            vec!["2026-09-20T00:00:00.002345+00:00\tprompt\n"]
        );
    }
}
