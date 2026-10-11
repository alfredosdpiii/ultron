//! Minimal terminal emulator for renderer tests: a screen grid plus scrollback, honoring the sequences the
//! renderers emit (cursor up/down/column, CR/LF with scrolling, EL 2, ED 0/2/3, CUP home). Styling is dropped.

#![allow(dead_code)]

pub struct VTerm {
    pub width: usize,
    pub height: usize,
    pub screen: Vec<Vec<char>>,
    pub scrollback: Vec<String>,
    pub row: usize,
    pub col: usize,
}

impl VTerm {
    pub fn new(width: usize, height: usize) -> Self {
        Self {
            width,
            height,
            screen: vec![vec![' '; width]; height],
            scrollback: Vec::new(),
            row: 0,
            col: 0,
        }
    }

    fn line_feed(&mut self) {
        if self.row + 1 < self.height {
            self.row += 1;
        } else {
            let top = self.screen.remove(0);
            self.scrollback
                .push(top.iter().collect::<String>().trim_end().to_string());
            self.screen.push(vec![' '; self.width]);
        }
    }

    pub fn write(&mut self, data: &str) {
        let chars: Vec<char> = data.chars().collect();
        let mut i = 0;
        while i < chars.len() {
            let c = chars[i];
            match c {
                '\x1b' => {
                    i += 1;
                    match chars.get(i) {
                        Some('[') => {
                            let start = i + 1;
                            let mut j = start;
                            while j < chars.len() && !(('@'..='~').contains(&chars[j])) {
                                j += 1;
                            }
                            let params: String = chars[start..j].iter().collect();
                            let n = params.parse::<usize>().unwrap_or(1).max(1);
                            match chars.get(j) {
                                Some('A') => self.row = self.row.saturating_sub(n),
                                Some('B') => self.row = (self.row + n).min(self.height - 1),
                                Some('G') => self.col = (n - 1).min(self.width - 1),
                                Some('H') => {
                                    self.row = 0;
                                    self.col = 0;
                                }
                                Some('K') => self.screen[self.row] = vec![' '; self.width],
                                Some('J') => match params.as_str() {
                                    "" | "0" => {
                                        for x in self.col..self.width {
                                            self.screen[self.row][x] = ' ';
                                        }
                                        for r in self.row + 1..self.height {
                                            self.screen[r] = vec![' '; self.width];
                                        }
                                    }
                                    "2" => self.screen = vec![vec![' '; self.width]; self.height],
                                    "3" => self.scrollback.clear(),
                                    _ => {}
                                },
                                _ => {}
                            }
                            i = j + 1;
                        }
                        Some(']') | Some('_') | Some('P') => {
                            // OSC / APC / DCS: skip to BEL or ST
                            i += 1;
                            while i < chars.len() {
                                if chars[i] == '\x07' {
                                    i += 1;
                                    break;
                                }
                                if chars[i] == '\x1b' && chars.get(i + 1) == Some(&'\\') {
                                    i += 2;
                                    break;
                                }
                                i += 1;
                            }
                        }
                        _ => i += 1,
                    }
                    continue;
                }
                '\r' => self.col = 0,
                '\n' => self.line_feed(),
                c => {
                    if self.col >= self.width {
                        self.col = 0;
                        self.line_feed();
                    }
                    self.screen[self.row][self.col] = c;
                    self.col += 1;
                }
            }
            i += 1;
        }
    }

    pub fn screen_lines(&self) -> Vec<String> {
        self.screen
            .iter()
            .map(|r| r.iter().collect::<String>().trim_end().to_string())
            .collect()
    }

    /// Scrollback followed by the screen, trailing blank screen rows removed.
    pub fn all_lines(&self) -> Vec<String> {
        let mut out = self.scrollback.clone();
        out.extend(self.screen_lines());
        while out.last().is_some_and(|l| l.is_empty()) {
            out.pop();
        }
        out
    }
}
