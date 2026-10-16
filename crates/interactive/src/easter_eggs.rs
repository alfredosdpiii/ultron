//! Pi's easter eggs (modes/interactive/components): `/arminsayshi`'s `ArminComponent` (an XBM portrait revealed by
//! one of seven animations, picked at random) and `/dementedelves`' `EarendilAnnouncementComponent`.

use std::rc::Rc;

use tui::components::{Component, Text};
use tui::theme::Theme;

use crate::components::{bold, fg};

/// XBM image: 31x36 pixels, LSB first, 1 = background, 0 = foreground.
const WIDTH: usize = 31;
const HEIGHT: usize = 36;
const BITS: [u8; 144] = [
    0xff, 0xff, 0xff, 0x7f, 0xff, 0xf0, 0xff, 0x7f, 0xff, 0xed, 0xff, 0x7f, 0xff, 0xdb, 0xff, 0x7f, 0xff, 0xb7, 0xff,
    0x7f, 0xff, 0x77, 0xfe, 0x7f, 0x3f, 0xf8, 0xfe, 0x7f, 0xdf, 0xff, 0xfe, 0x7f, 0xdf, 0x3f, 0xfc, 0x7f, 0x9f, 0xc3,
    0xfb, 0x7f, 0x6f, 0xfc, 0xf4, 0x7f, 0xf7, 0x0f, 0xf7, 0x7f, 0xf7, 0xff, 0xf7, 0x7f, 0xf7, 0xff, 0xe3, 0x7f, 0xf7,
    0x07, 0xe8, 0x7f, 0xef, 0xf8, 0x67, 0x70, 0x0f, 0xff, 0xbb, 0x6f, 0xf1, 0x00, 0xd0, 0x5b, 0xfd, 0x3f, 0xec, 0x53,
    0xc1, 0xff, 0xef, 0x57, 0x9f, 0xfd, 0xee, 0x5f, 0x9f, 0xfc, 0xae, 0x5f, 0x1f, 0x78, 0xac, 0x5f, 0x3f, 0x00, 0x50,
    0x6c, 0x7f, 0x00, 0xdc, 0x77, 0xff, 0xc0, 0x3f, 0x78, 0xff, 0x01, 0xf8, 0x7f, 0xff, 0x03, 0x9c, 0x78, 0xff, 0x07,
    0x8c, 0x7c, 0xff, 0x0f, 0xce, 0x78, 0xff, 0xff, 0xcf, 0x7f, 0xff, 0xff, 0xcf, 0x78, 0xff, 0xff, 0xdf, 0x78, 0xff,
    0xff, 0xdf, 0x7d, 0xff, 0xff, 0x3f, 0x7e, 0xff, 0xff, 0xff, 0x7f,
];
const BYTES_PER_ROW: usize = WIDTH.div_ceil(8);
/// Half-block rendering: two pixel rows per line.
const DISPLAY_HEIGHT: usize = HEIGHT.div_ceil(2);
const MESSAGE: &str = "ARMIN SAYS HI";

type Grid = Vec<Vec<char>>;

fn pixel(x: usize, y: usize) -> bool {
    if y >= HEIGHT {
        return false;
    }
    let byte = BITS[y * BYTES_PER_ROW + x / 8];
    (byte >> (x % 8)) & 1 == 0
}

fn cell(x: usize, row: usize) -> char {
    match (pixel(x, row * 2), pixel(x, row * 2 + 1)) {
        (true, true) => '█',
        (true, false) => '▀',
        (false, true) => '▄',
        (false, false) => ' ',
    }
}

fn final_grid() -> Grid {
    (0..DISPLAY_HEIGHT)
        .map(|row| (0..WIDTH).map(|x| cell(x, row)).collect())
        .collect()
}

fn empty_grid() -> Grid {
    vec![vec![' '; WIDTH]; DISPLAY_HEIGHT]
}

/// The effects, in `EFFECTS` order.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Effect {
    Typewriter,
    Scanline,
    Rain,
    Fade,
    Crt,
    Glitch,
    Dissolve,
}

const EFFECTS: [Effect; 7] = [
    Effect::Typewriter,
    Effect::Scanline,
    Effect::Rain,
    Effect::Fade,
    Effect::Crt,
    Effect::Glitch,
    Effect::Dissolve,
];

struct Drop {
    y: i64,
    settled: usize,
}

enum State {
    Position(usize),
    Row(usize),
    Rain(Vec<Drop>),
    /// Shuffled `(row, x)` positions and how many are shown.
    Reveal(Vec<(usize, usize)>, usize),
    Expansion(i64),
    Glitch {
        phase: u32,
        frames: u32,
    },
}

/// `Math.floor(Math.random() * n)`.
fn below(random: &mut dyn FnMut() -> f64, n: usize) -> usize {
    ((random() * n as f64).floor() as usize).min(n.saturating_sub(1))
}

/// Every `(row, x)` in row order, Fisher-Yates shuffled.
fn shuffled(random: &mut dyn FnMut() -> f64) -> Vec<(usize, usize)> {
    let mut positions: Vec<(usize, usize)> = (0..DISPLAY_HEIGHT)
        .flat_map(|row| (0..WIDTH).map(move |x| (row, x)))
        .collect();
    for i in (1..positions.len()).rev() {
        let j = below(random, i + 1);
        positions.swap(i, j);
    }
    positions
}

/// `ArminComponent`. It animates by frames ([`Armin::tick`]); [`Armin::fps`] says how often.
pub struct Armin {
    theme: Rc<Theme>,
    random: Box<dyn FnMut() -> f64>,
    pub effect: Effect,
    target: Grid,
    current: Grid,
    state: State,
    done: bool,
}

impl Armin {
    /// A new portrait: `random` is `Math.random` (it picks the effect and drives it).
    pub fn new(theme: Rc<Theme>, mut random: Box<dyn FnMut() -> f64>) -> Self {
        let effect = EFFECTS[below(&mut *random, EFFECTS.len())];
        let mut current = empty_grid();
        let state = match effect {
            Effect::Typewriter => State::Position(0),
            Effect::Scanline => State::Row(0),
            Effect::Rain => State::Rain(
                (0..WIDTH)
                    .map(|_| Drop {
                        y: -((random() * DISPLAY_HEIGHT as f64 * 2.0).floor() as i64),
                        settled: 0,
                    })
                    .collect(),
            ),
            Effect::Fade => State::Reveal(shuffled(&mut *random), 0),
            Effect::Crt => State::Expansion(0),
            Effect::Glitch => State::Glitch { phase: 0, frames: 8 },
            Effect::Dissolve => {
                const NOISE: [char; 7] = [' ', '░', '▒', '▓', '█', '▀', '▄'];
                current = (0..DISPLAY_HEIGHT)
                    .map(|_| (0..WIDTH).map(|_| NOISE[below(&mut *random, NOISE.len())]).collect())
                    .collect();
                State::Reveal(shuffled(&mut *random), 0)
            }
        };
        Armin {
            theme,
            random,
            effect,
            target: final_grid(),
            current,
            state,
            done: false,
        }
    }

    /// Frames per second (`startAnimation`).
    pub fn fps(&self) -> u64 {
        if self.effect == Effect::Glitch { 60 } else { 30 }
    }

    /// Whether the animation ended (`stopAnimation`).
    pub fn done(&self) -> bool {
        self.done
    }

    /// One frame (`tickEffect`); a finished animation stays as it is.
    pub fn tick(&mut self) {
        if self.done {
            return;
        }
        self.done = match &mut self.state {
            State::Position(pos) => {
                let mut done = false;
                for _ in 0..3 {
                    let (row, x) = (*pos / WIDTH, *pos % WIDTH);
                    if row >= DISPLAY_HEIGHT {
                        done = true;
                        break;
                    }
                    self.current[row][x] = self.target[row][x];
                    *pos += 1;
                }
                done
            }
            State::Row(row) => {
                if *row >= DISPLAY_HEIGHT {
                    true
                } else {
                    self.current[*row] = self.target[*row].clone();
                    *row += 1;
                    false
                }
            }
            State::Rain(drops) => {
                let mut settled_all = true;
                // Columns with nothing left to settle: their drop falls through once, then nothing changes.
                let mut falling_through = true;
                self.current = empty_grid();
                for (x, drop) in drops.iter_mut().enumerate() {
                    for row in (DISPLAY_HEIGHT - drop.settled.min(DISPLAY_HEIGHT))..DISPLAY_HEIGHT {
                        self.current[row][x] = self.target[row][x];
                    }
                    if drop.settled >= DISPLAY_HEIGHT {
                        continue;
                    }
                    settled_all = false;
                    let target = (0..DISPLAY_HEIGHT - drop.settled)
                        .rev()
                        .find(|row| self.target[*row][x] != ' ');
                    drop.y += 1;
                    if (0..DISPLAY_HEIGHT as i64).contains(&drop.y) {
                        let y = drop.y as usize;
                        match target {
                            Some(target) if y >= target => {
                                drop.settled = DISPLAY_HEIGHT - target;
                                drop.y = -((self.random)() * 5.0).floor() as i64 - 1;
                            }
                            _ => self.current[y][x] = '▓',
                        }
                    }
                    if target.is_some() || drop.y < DISPLAY_HEIGHT as i64 {
                        falling_through = false;
                    }
                }
                settled_all || falling_through
            }
            State::Reveal(positions, shown) => {
                let per_frame = if self.effect == Effect::Fade { 15 } else { 20 };
                let mut done = false;
                for _ in 0..per_frame {
                    let Some(&(row, x)) = positions.get(*shown) else {
                        done = true;
                        break;
                    };
                    self.current[row][x] = self.target[row][x];
                    *shown += 1;
                }
                done
            }
            State::Expansion(expansion) => {
                let mid = (DISPLAY_HEIGHT / 2) as i64;
                self.current = empty_grid();
                let (top, bottom) = (
                    (mid - *expansion).max(0),
                    (mid + *expansion).min(DISPLAY_HEIGHT as i64 - 1),
                );
                for row in top..=bottom {
                    self.current[row as usize] = self.target[row as usize].clone();
                }
                *expansion += 1;
                *expansion > DISPLAY_HEIGHT as i64
            }
            State::Glitch { phase, frames } => {
                if *phase < *frames {
                    let mut next = Vec::with_capacity(DISPLAY_HEIGHT);
                    for row in &self.target {
                        let offset = below(&mut *self.random, 7) as i64 - 3;
                        if (self.random)() < 0.3 {
                            // `row.slice(offset).concat(row.slice(0, offset))`, JavaScript's negative indices.
                            let at = offset.rem_euclid(WIDTH as i64) as usize;
                            let mut shifted = row[at..].to_vec();
                            shifted.extend_from_slice(&row[..at]);
                            next.push(shifted);
                        } else if (self.random)() < 0.2 {
                            next.push(self.target[below(&mut *self.random, DISPLAY_HEIGHT)].clone());
                        } else {
                            next.push(row.clone());
                        }
                    }
                    self.current = next;
                    *phase += 1;
                    false
                } else {
                    self.current = self.target.clone();
                    true
                }
            }
        };
    }
}

impl Component for Armin {
    fn render(&mut self, width: usize) -> Vec<String> {
        let available = width.saturating_sub(1);
        let line = |text: &str| {
            let pad = available.saturating_sub(text.chars().count());
            format!(" {}{}", fg(&self.theme, "accent", text), " ".repeat(pad))
        };
        let mut lines: Vec<String> = self
            .current
            .iter()
            .map(|row| line(&row.iter().take(available).collect::<String>()))
            .collect();
        lines.push(line(MESSAGE));
        lines
    }
}

/// A component the app keeps a handle on (to animate it) while the transcript shows it.
pub struct Shared<C>(pub Rc<std::cell::RefCell<C>>);

impl<C: Component> Component for Shared<C> {
    fn render(&mut self, width: usize) -> Vec<String> {
        self.0.borrow_mut().render(width)
    }
}

/// `Math.random` for a portrait: xorshift64* seeded from the clock.
pub fn seeded_random(now: std::time::Instant) -> impl FnMut() -> f64 {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos() as u64);
    let mut state = (nanos ^ (now.elapsed().as_nanos() as u64).rotate_left(17)) | 1;
    move || {
        state ^= state >> 12;
        state ^= state << 25;
        state ^= state >> 27;
        (state.wrapping_mul(0x2545_f491_4f6c_dd1d) >> 11) as f64 / (1u64 << 53) as f64
    }
}

const BLOG_URL: &str = "https://mariozechner.at/posts/2026-04-08-ive-sold-out/";
const IMAGE_FILENAME: &str = "clankolas.png";
/// The announcement's image (modes/interactive/assets/clankolas.png).
const IMAGE: &[u8] = include_bytes!("../../../packages/coding-agent/src/modes/interactive/assets/clankolas.png");

/// `EarendilAnnouncementComponent`, with its image as a terminal without image support shows it (this TUI draws
/// every image as its fallback line).
pub struct EarendilAnnouncement {
    theme: Rc<Theme>,
    image: tui::image::Image,
}

impl EarendilAnnouncement {
    pub fn new(theme: Rc<Theme>) -> Self {
        let muted = Rc::clone(&theme);
        let caps = tui::image::Capabilities {
            images: None,
            true_color: false,
            hyperlinks: false,
        };
        let mut image = tui::image::Image::new(
            &base64_encode(IMAGE),
            "image/png",
            Box::new(move |s: &str| fg(&muted, "muted", s)),
            caps,
        );
        image.max_width_cells = Some(56);
        image.filename = Some(IMAGE_FILENAME.into());
        EarendilAnnouncement { theme, image }
    }
}

fn base64_encode(bytes: &[u8]) -> String {
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let n = (u32::from(chunk[0]) << 16)
            | (u32::from(*chunk.get(1).unwrap_or(&0)) << 8)
            | u32::from(*chunk.get(2).unwrap_or(&0));
        for i in 0..4 {
            if i <= chunk.len() {
                out.push(TABLE[((n >> (18 - 6 * i)) & 63) as usize] as char);
            } else {
                out.push('=');
            }
        }
    }
    out
}

impl Component for EarendilAnnouncement {
    fn render(&mut self, width: usize) -> Vec<String> {
        let t = &self.theme;
        let border = fg(t, "accent", &"─".repeat(width.max(1)));
        let text = |s: &str| Text::new(s, 1, 0, None).render(width);
        let mut lines = vec![border.clone()];
        lines.extend(text(&bold(&fg(t, "accent", "pi has joined Earendil"))));
        lines.push(String::new());
        lines.extend(text(&fg(t, "muted", "Read the blog post:")));
        lines.extend(text(&fg(t, "mdLink", BLOG_URL)));
        lines.push(String::new());
        lines.extend(self.image.render(width));
        lines.push(String::new());
        lines.push(border);
        lines
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn interactive_easter_eggs_base64_matches_the_standard_alphabet() {
        assert_eq!(base64_encode(b""), "");
        assert_eq!(base64_encode(b"f"), "Zg==");
        assert_eq!(base64_encode(b"fo"), "Zm8=");
        assert_eq!(base64_encode(b"foo"), "Zm9v");
        assert_eq!(base64_encode(b"\xff\xfe\xfd\x00"), "//79AA==");
    }
}
