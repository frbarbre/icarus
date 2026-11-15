use std::fmt::Write as _;
use std::sync::{Arc, Mutex};

use alacritty_terminal::event::{Event, EventListener};
use alacritty_terminal::grid::Dimensions;
use alacritty_terminal::index::{Column, Line};
use alacritty_terminal::term::cell::{Cell, Flags};
use alacritty_terminal::term::test::TermSize;
use alacritty_terminal::term::{Config, Term};
use alacritty_terminal::vte::ansi::{Color, NamedColor, Processor, StdSyncHandler};

pub const SCROLLBACK_LINES: usize = 10_000;

#[derive(Clone, Default)]
struct Collector(Arc<Mutex<Vec<Event>>>);

impl EventListener for Collector {
    fn send_event(&self, event: Event) {
        if let Ok(mut events) = self.0.lock() {
            events.push(event);
        }
    }
}

/// What the emulator wants done after it processed output.
#[derive(Debug, Default, PartialEq)]
pub struct Effects {
    /// Replies to terminal queries (cursor position, device attributes) owed to the program.
    pub pty_replies: Vec<u8>,
    pub title: Option<Option<String>>,
    pub bell: bool,
}

/// Headless terminal state kept by the server so it can answer "what is on screen" without a client.
pub struct Emulator {
    term: Term<Collector>,
    parser: Processor<StdSyncHandler>,
    events: Collector,
    title: Option<String>,
}

impl Emulator {
    pub fn new(columns: u16, rows: u16) -> Self {
        let events = Collector::default();
        let config = Config {
            scrolling_history: SCROLLBACK_LINES,
            ..Config::default()
        };
        let size = TermSize::new(usize::from(columns.max(2)), usize::from(rows.max(1)));
        Self {
            term: Term::new(config, &size, events.clone()),
            parser: Processor::new(),
            events,
            title: None,
        }
    }

    pub fn feed(&mut self, bytes: &[u8]) -> Effects {
        self.parser.advance(&mut self.term, bytes);
        let drained = match self.events.0.lock() {
            Ok(mut events) => std::mem::take(&mut *events),
            Err(_) => Vec::new(),
        };
        let mut effects = Effects::default();
        for event in drained {
            match event {
                Event::PtyWrite(text) => effects.pty_replies.extend_from_slice(text.as_bytes()),
                Event::Title(title) => {
                    self.title = Some(title.clone());
                    effects.title = Some(Some(title));
                }
                Event::ResetTitle => {
                    self.title = None;
                    effects.title = Some(None);
                }
                Event::Bell => effects.bell = true,
                _ => {}
            }
        }
        effects
    }

    /// Rebuilds a screen from `snapshot_vt` output, e.g. after another server handed it over.
    pub fn restore(&mut self, snapshot: &[u8], title: Option<String>) {
        self.feed(snapshot);
        self.title = title;
    }

    pub fn resize(&mut self, columns: u16, rows: u16) {
        let size = TermSize::new(usize::from(columns.max(2)), usize::from(rows.max(1)));
        self.term.resize(size);
    }

    pub fn title(&self) -> Option<&str> {
        self.title.as_deref()
    }

    pub fn size(&self) -> (u16, u16) {
        let grid = self.term.grid();
        (
            u16::try_from(grid.columns()).unwrap_or(u16::MAX),
            u16::try_from(grid.screen_lines()).unwrap_or(u16::MAX),
        )
    }

    /// The visible screen as plain text, one line per row, trailing blanks trimmed.
    pub fn screen_text(&self) -> String {
        let grid = self.term.grid();
        let lines = grid.screen_lines() as i32;
        self.lines_text(0, lines)
    }

    /// Scrollback plus screen as plain text.
    pub fn full_text(&self) -> String {
        let grid = self.term.grid();
        let top = -(grid.history_size() as i32);
        self.lines_text(top, grid.screen_lines() as i32)
    }

    fn lines_text(&self, from: i32, to: i32) -> String {
        let grid = self.term.grid();
        let mut out = String::new();
        let mut pending_blank = 0usize;
        for line in from..to {
            let row = &grid[Line(line)];
            let mut text = String::new();
            for column in 0..grid.columns() {
                let cell = &row[Column(column)];
                if cell.flags.contains(Flags::WIDE_CHAR_SPACER) {
                    continue;
                }
                text.push(cell.c);
            }
            let trimmed = text.trim_end();
            if trimmed.is_empty() {
                pending_blank += 1;
                continue;
            }
            for _ in 0..pending_blank {
                out.push('\n');
            }
            pending_blank = 0;
            out.push_str(trimmed);
            out.push('\n');
        }
        out
    }

    /// Serializes scrollback and screen as VT sequences a fresh emulator of the same size can replay.
    pub fn snapshot_vt(&self) -> String {
        let grid = self.term.grid();
        let top = -(grid.history_size() as i32);
        let bottom = grid.screen_lines() as i32;
        let mut out = String::from("\x1b[0m\x1b[H\x1b[2J\x1b[3J");
        for line in top..bottom {
            let row = &grid[Line(line)];
            let mut last = SgrState::default();
            let mut text_end = grid.columns();
            while text_end > 0 && is_blank(&row[Column(text_end - 1)]) {
                text_end -= 1;
            }
            for column in 0..text_end {
                let cell = &row[Column(column)];
                if cell.flags.contains(Flags::WIDE_CHAR_SPACER) {
                    continue;
                }
                let state = SgrState::of(cell);
                if state != last {
                    state.write(&mut out);
                    last = state;
                }
                out.push(cell.c);
            }
            out.push_str("\x1b[0m");
            if line + 1 < bottom {
                let wraps = text_end == grid.columns()
                    && row[Column(grid.columns() - 1)]
                        .flags
                        .contains(Flags::WRAPLINE);
                if !wraps {
                    out.push_str("\r\n");
                }
            }
        }
        let cursor = self.term.grid().cursor.point;
        let _ = write!(out, "\x1b[{};{}H", cursor.line.0 + 1, cursor.column.0 + 1);
        out
    }
}

fn is_blank(cell: &Cell) -> bool {
    cell.c == ' ' && SgrState::of(cell) == SgrState::default()
}

#[derive(Clone, Copy, PartialEq, Eq)]
struct SgrState {
    fg: Color,
    bg: Color,
    flags: Flags,
}

impl Default for SgrState {
    fn default() -> Self {
        Self {
            fg: Color::Named(NamedColor::Foreground),
            bg: Color::Named(NamedColor::Background),
            flags: Flags::empty(),
        }
    }
}

impl SgrState {
    fn of(cell: &Cell) -> Self {
        let style = Flags::BOLD
            | Flags::DIM
            | Flags::ITALIC
            | Flags::UNDERLINE
            | Flags::INVERSE
            | Flags::HIDDEN
            | Flags::STRIKEOUT;
        Self {
            fg: cell.fg,
            bg: cell.bg,
            flags: cell.flags & style,
        }
    }

    fn write(&self, out: &mut String) {
        out.push_str("\x1b[0");
        for (flag, code) in [
            (Flags::BOLD, "1"),
            (Flags::DIM, "2"),
            (Flags::ITALIC, "3"),
            (Flags::UNDERLINE, "4"),
            (Flags::INVERSE, "7"),
            (Flags::HIDDEN, "8"),
            (Flags::STRIKEOUT, "9"),
        ] {
            if self.flags.contains(flag) {
                out.push(';');
                out.push_str(code);
            }
        }
        write_color(out, self.fg, true);
        write_color(out, self.bg, false);
        out.push('m');
    }
}

fn write_color(out: &mut String, color: Color, foreground: bool) {
    let (base, bright_base, extended) = if foreground {
        (30, 90, 38)
    } else {
        (40, 100, 48)
    };
    match color {
        Color::Named(named) => {
            let index = named as usize;
            if index < 8 {
                let _ = write!(out, ";{}", base + index);
            } else if index < 16 {
                let _ = write!(out, ";{}", bright_base + index - 8);
            }
        }
        Color::Indexed(index) => {
            let _ = write!(out, ";{extended};5;{index}");
        }
        Color::Spec(rgb) => {
            let _ = write!(out, ";{extended};2;{};{};{}", rgb.r, rgb.g, rgb.b);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn screen_text_reflects_output_and_cursor_movement() {
        let mut emulator = Emulator::new(20, 4);
        emulator.feed(b"hello\r\nworld\x1b[1;1HJ");
        assert_eq!(emulator.screen_text(), "Jello\nworld\n");
    }

    #[test]
    fn snapshot_replays_to_identical_screen_with_colors() {
        let mut source = Emulator::new(30, 5);
        source.feed(b"\x1b[1;31mred bold\x1b[0m plain\r\n\x1b[38;5;200mindexed\x1b[0m\r\nthird");
        let snapshot = source.snapshot_vt();

        let mut replica = Emulator::new(30, 5);
        replica.feed(snapshot.as_bytes());
        assert_eq!(replica.screen_text(), source.screen_text());
        assert_eq!(replica.snapshot_vt(), snapshot);
    }

    #[test]
    fn scrollback_survives_in_full_text() {
        let mut emulator = Emulator::new(10, 3);
        for line in 0..10 {
            emulator.feed(format!("line{line}\r\n").as_bytes());
        }
        let full = emulator.full_text();
        assert!(full.starts_with("line0\n"), "{full}");
        assert!(full.contains("line9"));
        assert!(!emulator.screen_text().contains("line0"));
    }

    #[test]
    fn device_status_query_produces_a_reply() {
        let mut emulator = Emulator::new(10, 3);
        let effects = emulator.feed(b"\x1b[6n");
        assert_eq!(effects.pty_replies, b"\x1b[1;1R");
    }

    #[test]
    fn title_and_bell_are_reported() {
        let mut emulator = Emulator::new(10, 3);
        let effects = emulator.feed(b"\x1b]0;build\x07\x07");
        assert_eq!(effects.title, Some(Some("build".into())));
        assert!(effects.bell);
        assert_eq!(emulator.title(), Some("build"));
    }
}
