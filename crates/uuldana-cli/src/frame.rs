//! uuldana's own screen: a header that is always there and, below it, the
//! agent's terminal.
//!
//! The agent writes to a pseudo terminal that is smaller than the real one by
//! the rows of the header. What it writes goes into an in-memory terminal
//! ([`vt100`]) and only the rows that changed are drawn under the header.
//! Nothing the agent sends reaches the real terminal as it is, so an agent that
//! clears the screen, switches to its alternate screen or repaints after a
//! resize cannot touch the header.
//!
//! The whole thing runs on the real terminal's alternate screen, as a full
//! screen program does, and gives the terminal back as it was on exit.

use std::{
    io::{self, Write},
    num::NonZeroU16,
    sync::{Arc, Mutex, MutexGuard},
    time::{Duration, Instant},
};

use uuldana_core::TerminalSize;

use crate::banner::{EARLY, HINT, SUBTITLE, TITLE};

/// Rows of history the in-memory terminal keeps for the agent.
const HISTORY_ROWS: usize = 10_000;
/// Lines of the agent's screen that go to the next agent in a handoff.
const TRANSCRIPT_LINES: usize = 120;
/// Rows the full header takes: the title, a line for the agent, a line for
/// notes and a rule.
const FULL_HEADER_ROWS: u16 = 9;
/// The full header needs a terminal at least this tall and wide.
const FULL_MIN_ROWS: u16 = 34;
const FULL_MIN_COLS: u16 = 62;
/// Greys for words and for the faded top of the title. The terminal's own
/// bright black is close to invisible in many color schemes.
const GREY: &str = "38;5;246";
const DIM_GREY: &str = "38;5;241";
/// Below this many rows there is no room for a header at all.
const COMPACT_MIN_ROWS: u16 = 6;

/// Rows the header takes on a terminal of this size.
pub(crate) fn header_rows(rows: u16, cols: u16) -> u16 {
    if rows >= FULL_MIN_ROWS && cols >= FULL_MIN_COLS {
        FULL_HEADER_ROWS
    } else if rows >= COMPACT_MIN_ROWS {
        1
    } else {
        0
    }
}

/// What the agent's terminal reports about itself, besides its contents.
#[derive(Default)]
struct Reports {
    title: String,
}

impl vt100::Callbacks for Reports {
    fn set_window_title(&mut self, _: &mut vt100::Screen, title: &[u8]) {
        self.title = String::from_utf8_lossy(title).into_owned();
    }
}

struct State {
    out: Box<dyn Write + Send>,
    parser: vt100::Parser<Reports>,
    /// The real terminal.
    rows: u16,
    cols: u16,
    header: u16,
    color: bool,
    agent: String,
    badge: Option<String>,
    status: String,
    /// Rows of history in view; zero is the live screen.
    scrolled: usize,
    /// The body rows as they are on the real terminal. Empty until drawn.
    drawn: Vec<String>,
    title_drawn: String,
    /// Whether the real terminal has been told to bracket pastes.
    paste_drawn: bool,
    /// The agent's console has announced that it takes keys as the Windows
    /// console reports them (`ESC [ ? 9001 h`).
    win32_keys: bool,
    /// The end of what the agent last wrote, in case the announcement is split
    /// across two writes.
    win32_carry: Vec<u8>,
    dirty: bool,
    finished: bool,
    /// The user has sent something to this agent, or it was given a first
    /// prompt: there is a conversation to carry on.
    spoke: bool,
    /// When the agent last wrote, `None` before it has written anything.
    last_fed: Option<Instant>,
}

/// uuldana's screen. Cheap to clone; every clone is the same screen.
#[derive(Clone)]
pub(crate) struct Frame {
    state: Arc<Mutex<State>>,
}

fn non_zero(value: u16) -> NonZeroU16 {
    NonZeroU16::new(value.max(1)).expect("at least one")
}

impl Frame {
    /// Takes over the terminal and draws the header. `badge` is a warning that
    /// stays in the header for the whole session.
    pub(crate) fn start(
        out: Box<dyn Write + Send>,
        size: TerminalSize,
        color: bool,
        badge: Option<String>,
    ) -> Self {
        let (rows, cols) = (size.rows, size.cols);
        let header = header_rows(rows, cols);
        let parser = vt100::Parser::new_with_callbacks(
            non_zero(rows.saturating_sub(header)),
            non_zero(cols),
            HISTORY_ROWS,
            Reports::default(),
        );
        let mut state = State {
            out,
            parser,
            rows,
            cols,
            header,
            color,
            agent: String::new(),
            badge,
            status: String::new(),
            scrolled: 0,
            drawn: Vec::new(),
            title_drawn: String::new(),
            paste_drawn: false,
            win32_keys: false,
            win32_carry: Vec::new(),
            dirty: false,
            finished: false,
            spoke: false,
            last_fed: None,
        };
        let _ = state.out.write_all(b"\x1b[?1049h\x1b[2J");
        state.paint(true);
        Self {
            state: Arc::new(Mutex::new(state)),
        }
    }

    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// The size the agent's terminal has: the real terminal less the header.
    #[cfg(test)]
    pub(crate) fn body_size(&self) -> TerminalSize {
        let state = self.lock();
        TerminalSize::new(state.rows - state.header, state.cols)
    }

    /// Follows the real terminal's size and returns the size for the agent's
    /// terminal. Draws everything again when the size changed.
    pub(crate) fn sync(&self, real: TerminalSize) -> TerminalSize {
        let mut state = self.lock();
        if (real.rows, real.cols) != (state.rows, state.cols) {
            state.rows = real.rows;
            state.cols = real.cols;
            state.header = header_rows(real.rows, real.cols);
            let body = non_zero(real.rows.saturating_sub(state.header));
            state.parser.set_size(body, non_zero(real.cols));
            state.scrolled = state.parser.screen().scrollback();
            state.out_write(b"\x1b[2J");
            state.paint(true);
        }
        TerminalSize::new(state.rows - state.header, state.cols)
    }

    /// An agent has started: its name goes in the header and it gets a clean
    /// screen. A new pseudo terminal takes the screen to be empty and the cursor
    /// to be at the top, so what the agent before it left is cleared.
    pub(crate) fn set_agent(&self, name: &str, first_prompt: bool) {
        let mut state = self.lock();
        state.spoke = first_prompt;
        state.last_fed = None;
        if !state.agent.is_empty() {
            state.parser.process(
                // A new agent starts with the screen empty and with no mode
                // the one before it asked for: alternate screen, mouse
                // events, pastes.
                b"\x1b[0m\x1b[?1049l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1005l\
                      \x1b[?1006l\x1b[?2004l\x1b[?25h\x1b[r\x1b[2J\x1b[3J\x1b[H",
            );
            state.parser.screen_mut().set_scrollback(0);
            state.scrolled = 0;
        }
        // The console of the new agent announces itself when it starts.
        state.win32_keys = false;
        state.win32_carry.clear();
        state.agent = name.to_owned();
        state.dirty = false;
        state.paint(true);
    }

    /// A line of news for the header, replacing the one before it.
    pub(crate) fn set_status(&self, text: &str) {
        let mut state = self.lock();
        state.status = text.to_owned();
        state.paint_header();
    }

    /// The agent's cursor as 1-based `(row, column)` in its own terminal, which
    /// is what its pseudo terminal asks the host for.
    pub(crate) fn cursor(&self) -> (u16, u16) {
        let (row, col) = self.lock().parser.screen().cursor_position();
        (row.saturating_add(1), col.saturating_add(1))
    }

    /// Looks back into the agent's history by this many rows; negative goes
    /// forward again.
    pub(crate) fn scroll(&self, rows: isize) {
        let mut state = self.lock();
        let target = state.scrolled.saturating_add_signed(rows);
        state.set_scrolled(target);
    }

    /// Back to the live screen. True when it was somewhere else.
    pub(crate) fn to_live(&self) -> bool {
        let mut state = self.lock();
        let was = state.scrolled > 0;
        if was {
            state.set_scrolled(0);
        }
        was
    }

    /// The plain text the agent's screen showed, history first and the live
    /// screen last, without the header. At most the last 120 lines with text.
    pub(crate) fn transcript(&self) -> String {
        let mut state = self.lock();
        let was = state.parser.screen().scrollback();
        let width = state.parser.screen().size().1.get();
        let mut lines: Vec<String> = Vec::new();
        let mut back = state.parser.screen().scrollback_len();
        while back > 0 {
            state.parser.screen_mut().set_scrollback(back);
            let page: Vec<String> = state.parser.screen().rows(0, width).collect();
            let take = page.len().min(back);
            lines.extend(page.into_iter().take(take));
            back -= take;
        }
        state.parser.screen_mut().set_scrollback(0);
        lines.extend(state.parser.screen().rows(0, width));
        state.parser.screen_mut().set_scrollback(was);
        let mut kept: Vec<&str> = lines.iter().map(|line| line.trim_end()).collect();
        kept.retain(|line| !line.trim().is_empty());
        let from = kept.len().saturating_sub(TRANSCRIPT_LINES);
        kept[from..].join("\n")
    }

    /// Whether the agent has asked the terminal for mouse events, and if so
    /// whether it wants them in the SGR encoding.
    pub(crate) fn mouse_tracking(&self) -> Option<bool> {
        let state = self.lock();
        let screen = state.parser.screen();
        (screen.mouse_protocol_mode() != vt100::MouseProtocolMode::None)
            .then(|| screen.mouse_protocol_encoding() == vt100::MouseProtocolEncoding::Sgr)
    }

    /// Rows above the agent's area, the header.
    pub(crate) fn body_top(&self) -> u16 {
        self.lock().header
    }

    /// The agent is drawing on the alternate screen of its own.
    pub(crate) fn on_alternate_screen(&self) -> bool {
        self.lock().parser.screen().alternate_screen()
    }

    /// Rows a page up or down moves.
    pub(crate) fn page(&self) -> isize {
        let state = self.lock();
        isize::try_from(state.rows.saturating_sub(state.header).max(2) / 2).unwrap_or(1)
    }

    /// Gives the real terminal back as it was.
    pub(crate) fn finish(&self) {
        let mut state = self.lock();
        if !state.finished {
            state.finished = true;
            state.out_write(b"\x1b[0m\x1b[?25h\x1b[?2004l\x1b[?1049l");
        }
    }

    /// The agent's console takes keys as the Windows console reports them.
    pub(crate) fn takes_win32_keys(&self) -> bool {
        self.lock().win32_keys
    }

    fn feed(&self, bytes: &[u8]) {
        let mut state = self.lock();
        state.note_win32_keys(bytes);
        state.parser.process(bytes);
        state.dirty = true;
        state.last_fed = Some(Instant::now());
    }

    /// Notes what the user sent to the agent: a line sent makes a conversation.
    pub(crate) fn note_sent(&self, bytes: &[u8]) {
        if bytes.contains(&b'\r') {
            self.lock().spoke = true;
        }
    }

    /// There is a conversation with the agent on the screen.
    pub(crate) fn spoke(&self) -> bool {
        self.lock().spoke
    }

    /// The agent has asked for pastes to be sent bracketed.
    pub(crate) fn takes_paste(&self) -> bool {
        self.lock().parser.screen().bracketed_paste()
    }

    /// The agent has drawn its screen, has been quiet for `quiet` and takes
    /// pastes, so text can be put in its input box.
    pub(crate) fn ready_for_paste(&self, quiet: Duration) -> bool {
        let state = self.lock();
        state.last_fed.is_some_and(|at| at.elapsed() >= quiet)
            && state.parser.screen().bracketed_paste()
    }

    fn render(&self) {
        let mut state = self.lock();
        if state.dirty {
            state.dirty = false;
            state.paint(false);
        }
    }
}

impl State {
    /// Follows the agent's console announcing, or ending, that it takes keys as
    /// the Windows console reports them.
    fn note_win32_keys(&mut self, bytes: &[u8]) {
        const ON: &[u8] = b"\x1b[?9001h";
        const OFF: &[u8] = b"\x1b[?9001l";
        let mut data = std::mem::take(&mut self.win32_carry);
        data.extend_from_slice(bytes);
        let last = |what: &[u8]| data.windows(what.len()).rposition(|w| w == what);
        match (last(ON), last(OFF)) {
            (Some(on), Some(off)) => self.win32_keys = on > off,
            (Some(_), None) => self.win32_keys = true,
            (None, Some(_)) => self.win32_keys = false,
            (None, None) => {}
        }
        let keep = data.len().min(ON.len() - 1);
        self.win32_carry = data[data.len() - keep..].to_vec();
    }

    fn out_write(&mut self, bytes: &[u8]) {
        let _ = self.out.write_all(bytes);
        let _ = self.out.flush();
    }

    fn set_scrolled(&mut self, rows: usize) {
        self.parser.screen_mut().set_scrollback(rows);
        self.scrolled = self.parser.screen().scrollback();
        self.paint_header();
        self.dirty = false;
        self.paint(true);
    }

    /// Draws what changed, or everything when `full`.
    fn paint(&mut self, full: bool) {
        let mut text = String::from("\x1b[?2026h\x1b[?25l");
        if full {
            self.drawn.clear();
            text.push_str(&self.header_text());
        }
        self.body_text(&mut text, full);
        let title = self.parser.callbacks().title.clone();
        if title != self.title_drawn {
            text.push_str(&format!("\x1b]0;{title}\x07"));
            self.title_drawn = title;
        }
        // The terminal brackets pastes when the agent asked for it. Without
        // that, a terminal warns before it pastes more than one line.
        let paste = self.parser.screen().bracketed_paste();
        if paste != self.paste_drawn {
            text.push_str(if paste { "\x1b[?2004h" } else { "\x1b[?2004l" });
            self.paste_drawn = paste;
        }
        self.cursor_text(&mut text);
        text.push_str("\x1b[?2026l");
        self.out_write(text.as_bytes());
    }

    fn paint_header(&mut self) {
        if self.header == 0 {
            return;
        }
        let mut text = String::from("\x1b[?2026h\x1b[?25l");
        text.push_str(&self.header_text());
        self.cursor_text(&mut text);
        text.push_str("\x1b[?2026l");
        self.out_write(text.as_bytes());
    }

    fn body_text(&mut self, text: &mut String, full: bool) {
        let screen = self.parser.screen();
        let rows: Vec<String> = screen.rows_formatted(0, self.cols).collect();
        for (index, row) in rows.iter().enumerate() {
            if full || self.drawn.get(index) != Some(row) {
                let at = usize::from(self.header) + index + 1;
                // The row is cleared first: empty cells are skipped over, not
                // written, so whatever was there before would stay.
                text.push_str(&format!("\x1b[{at};1H\x1b[0m\x1b[2K{row}\x1b[0m"));
            }
        }
        self.drawn = rows;
    }

    fn cursor_text(&self, text: &mut String) {
        let screen = self.parser.screen();
        if self.scrolled > 0 || screen.hide_cursor() {
            return;
        }
        let (row, col) = screen.cursor_position();
        let at = usize::from(self.header) + usize::from(row) + 1;
        text.push_str(&format!("\x1b[{at};{}H\x1b[?25h", usize::from(col) + 1));
    }

    fn sgr(&self, code: &str) -> String {
        if self.color {
            format!("\x1b[{code}m")
        } else {
            String::new()
        }
    }

    /// The header, one line per row, each placed and cleared to its end.
    fn header_text(&self) -> String {
        let lines = self.header_lines();
        let mut text = String::new();
        for (index, line) in lines.iter().enumerate() {
            text.push_str(&format!("\x1b[{};1H\x1b[0m{line}\x1b[0m\x1b[K", index + 1));
        }
        text
    }

    fn note(&self) -> String {
        if self.scrolled > 0 {
            format!(
                "{} rows back in the history. Scroll down or Shift+PageDown goes forward, any key returns.",
                self.scrolled
            )
        } else if !self.status.is_empty() {
            self.status.clone()
        } else {
            HINT.to_owned()
        }
    }

    fn header_lines(&self) -> Vec<String> {
        let cols = usize::from(self.cols);
        let reset = self.sgr("0");
        let badge = self
            .badge
            .as_deref()
            .map(|badge| format!("  {}{badge}{reset}", self.sgr("1;31")))
            .unwrap_or_default();
        let agent = if self.agent.is_empty() {
            String::new()
        } else {
            format!(
                "  {}now: {}{}{reset}",
                self.sgr(GREY),
                self.sgr("1;97"),
                self.agent
            )
        };
        match self.header {
            0 => Vec::new(),
            1 => {
                let text = format!(
                    " UULDANA  {}  {}{}{}",
                    EARLY.to_lowercase(),
                    if self.agent.is_empty() {
                        String::new()
                    } else {
                        format!("{}  ", self.agent)
                    },
                    self.badge
                        .as_deref()
                        .map(|badge| format!("{badge}  "))
                        .unwrap_or_default(),
                    self.note()
                );
                let text = fit(&text, cols);
                vec![format!("{}{text:<cols$}{reset}", self.sgr("7"))]
            }
            _ => {
                let mut lines = Vec::new();
                for (index, row) in TITLE.iter().enumerate() {
                    let color = self.sgr(if index < 2 { DIM_GREY } else { "31" });
                    lines.push(format!("{color}{}{reset}", fit(row, cols)));
                }
                lines.push(format!(
                    "{}{SUBTITLE}{reset}  {}{EARLY}{reset}{agent}{badge}",
                    self.sgr("36"),
                    self.sgr("33")
                ));
                lines.push(format!(
                    "{}{}{reset}",
                    self.sgr(GREY),
                    fit(&self.note(), cols)
                ));
                lines.push(format!("{}{}{reset}", self.sgr(DIM_GREY), "─".repeat(cols)));
                lines
            }
        }
    }
}

/// `text` cut to `width` characters.
fn fit(text: &str, width: usize) -> String {
    text.chars().take(width).collect()
}

/// Where the agent's output goes: into the agent's terminal, drawn when the
/// relay flushes.
pub(crate) struct FrameWriter(pub(crate) Frame);

impl Write for FrameWriter {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.0.feed(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        self.0.render();
        Ok(())
    }
}

/// Gives the terminal back when dropped, including when a session ends with an
/// error or a panic.
pub(crate) struct Restore(pub(crate) Frame);

impl Drop for Restore {
    fn drop(&mut self) {
        self.0.finish();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Collects what the frame writes to the real terminal.
    #[derive(Clone, Default)]
    struct Sink(Arc<Mutex<Vec<u8>>>);

    impl Write for Sink {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    /// A real terminal in miniature: shows what the frame wrote.
    struct Screen {
        sink: Sink,
        terminal: vt100::Parser,
        taken: usize,
    }

    impl Screen {
        fn new(rows: u16, cols: u16) -> (Self, Frame) {
            let sink = Sink::default();
            let frame = Frame::start(
                Box::new(sink.clone()),
                TerminalSize::new(rows, cols),
                false,
                None,
            );
            let mut screen = Self {
                sink,
                terminal: vt100::Parser::new(non_zero(rows), non_zero(cols), 0),
                taken: 0,
            };
            screen.settle();
            (screen, frame)
        }

        fn settle(&mut self) {
            let bytes = self.sink.0.lock().unwrap().clone();
            self.terminal.process(&bytes[self.taken..]);
            self.taken = bytes.len();
        }

        fn rows(&mut self) -> Vec<String> {
            self.settle();
            self.terminal
                .screen()
                .contents()
                .split('\n')
                .map(|row| row.trim_end().to_owned())
                .collect()
        }

        fn row(&mut self, index: usize) -> String {
            self.rows().get(index).cloned().unwrap_or_default()
        }
    }

    fn agent_writes(frame: &Frame, text: &str) {
        let mut writer = FrameWriter(frame.clone());
        writer.write_all(text.as_bytes()).unwrap();
        writer.flush().unwrap();
    }

    #[test]
    fn the_terminal_brackets_pastes_while_the_agent_asks_for_it() {
        let (mut screen, frame) = Screen::new(40, 100);
        screen.settle();
        assert!(!screen.terminal.screen().bracketed_paste());
        agent_writes(&frame, "\x1b[?2004hprompt");
        screen.settle();
        assert!(screen.terminal.screen().bracketed_paste());
        agent_writes(&frame, "\x1b[?2004l");
        screen.settle();
        assert!(!screen.terminal.screen().bracketed_paste());
        // The terminal is handed back the way it was found.
        agent_writes(&frame, "\x1b[?2004h");
        frame.finish();
        screen.settle();
        assert!(!screen.terminal.screen().bracketed_paste());
    }

    #[test]
    fn the_header_size_follows_the_terminal() {
        assert_eq!(header_rows(40, 120), FULL_HEADER_ROWS);
        assert_eq!(header_rows(33, 120), 1);
        assert_eq!(header_rows(40, 61), 1);
        assert_eq!(header_rows(6, 120), 1);
        assert_eq!(header_rows(5, 120), 0);
    }

    #[test]
    fn the_title_and_the_hint_are_drawn_before_any_agent_is_running() {
        let (mut screen, frame) = Screen::new(40, 100);
        let rows = screen.rows();
        assert_eq!(rows[0], TITLE[0].trim_end());
        assert_eq!(rows[5], TITLE[5].trim_end());
        assert_eq!(rows[7], HINT);
        assert!(rows[8].starts_with('─'));
        assert_eq!(frame.body_size(), TerminalSize::new(40 - 9, 100));
    }

    #[test]
    fn the_agent_is_drawn_under_the_header() {
        let (mut screen, frame) = Screen::new(40, 100);
        agent_writes(&frame, "first line\r\nsecond line");
        assert_eq!(screen.row(9), "first line");
        assert_eq!(screen.row(10), "second line");
        assert_eq!(screen.row(0), TITLE[0].trim_end());
    }

    #[test]
    fn an_agent_that_clears_its_screen_leaves_the_header_alone() {
        let (mut screen, frame) = Screen::new(40, 100);
        agent_writes(&frame, "old\r\nold\r\nold");
        agent_writes(&frame, "\x1b[2J\x1b[3J\x1b[Hnew start");
        let rows = screen.rows();
        assert_eq!(rows[0], TITLE[0].trim_end());
        assert_eq!(rows[7], HINT);
        assert_eq!(rows[9], "new start");
        // What was on the agent's screen before is gone, the header is not.
        assert_eq!(rows.get(10).map_or("", String::as_str), "");
    }

    #[test]
    fn the_menu_on_the_alternate_screen_gives_the_agent_screen_back() {
        let (mut screen, frame) = Screen::new(40, 100);
        agent_writes(&frame, "the agent's work\r\nmore work");
        agent_writes(&frame, "\x1b[?1049h\x1b[H\x1b[2JAgent order\r\n> 1. one");
        assert_eq!(screen.row(9), "Agent order");
        assert_eq!(screen.row(0), TITLE[0].trim_end());
        agent_writes(&frame, "\x1b[?1049l");
        assert_eq!(screen.row(9), "the agent's work");
        assert_eq!(screen.row(10), "more work");
        assert_eq!(screen.row(0), TITLE[0].trim_end());
    }

    #[test]
    fn letters_of_an_earlier_screen_do_not_stay_under_empty_cells() {
        let (mut screen, frame) = Screen::new(40, 100);
        // The agent's row is mostly empty cells.
        agent_writes(&frame, "ab\x1b[8Gcd");
        agent_writes(
            &frame,
            "\x1b[?1049h\x1b[H\x1b[2JAgent order (first agent to use)",
        );
        assert_eq!(screen.row(9), "Agent order (first agent to use)");
        agent_writes(&frame, "\x1b[?1049l");
        assert_eq!(screen.row(9), "ab     cd");
    }

    #[test]
    fn a_new_agent_starts_on_an_empty_screen() {
        let (mut screen, frame) = Screen::new(40, 100);
        frame.set_agent("agent-b", false);
        agent_writes(
            &frame,
            "agent-b screen
\x1b[20;5Hlower part\x1b[?25l",
        );
        frame.set_agent("agent-a", false);
        // Nothing of the old screen is left and the cursor is where a new
        // pseudo terminal expects it.
        let rows = screen.rows();
        assert_eq!(rows[0], TITLE[0].trim_end());
        assert!(rows.iter().skip(9).all(|row| row.is_empty()), "{rows:?}");
        assert_eq!(frame.cursor(), (1, 1));
        assert!(screen.row(6).contains("now: agent-a"));
        assert!(
            screen.row(6).contains("EARLY ACCESS"),
            "{:?}",
            screen.row(6)
        );
        // The new agent draws as it would on a fresh terminal.
        agent_writes(&frame, "agent-a screen");
        assert_eq!(screen.row(9), "agent-a screen");
    }

    #[test]
    fn the_text_of_the_agents_screen_includes_what_scrolled_away_but_not_the_header() {
        let (_screen, frame) = Screen::new(40, 100);
        agent_writes(&frame, "Catat dulu: nomor kasus 7731\r\nOke.\r\n");
        let lines: String = (1..=80).map(|n| format!("line {n}\r\n")).collect();
        agent_writes(&frame, &lines);
        let text = frame.transcript();
        assert!(text.contains("Catat dulu: nomor kasus 7731"), "{text}");
        assert!(text.contains("line 80"), "{text}");
        assert!(!text.contains("ONE TERMINAL"), "{text}");
        assert!(text.lines().count() <= TRANSCRIPT_LINES);
        // Reading it leaves the view where it was.
        frame.scroll(10);
        let _ = frame.transcript();
        assert!(frame.to_live());
    }

    #[test]
    fn only_rows_that_changed_are_sent_again() {
        let (screen, frame) = Screen::new(40, 100);
        agent_writes(&frame, "one\r\ntwo\r\nthree");
        let before = screen.sink.0.lock().unwrap().len();
        agent_writes(&frame, "\x1b[2;1HTWO");
        let sent = screen.sink.0.lock().unwrap().len() - before;
        assert!(sent < 200, "sent {sent} bytes for one changed row");
    }

    #[test]
    fn a_resize_redraws_everything_and_gives_the_agent_the_new_size() {
        let (mut screen, frame) = Screen::new(40, 100);
        agent_writes(&frame, "kept text");
        let body = frame.sync(TerminalSize::new(44, 120));
        assert_eq!(body, TerminalSize::new(44 - 9, 120));
        screen.terminal.set_size(non_zero(44), non_zero(120));
        let rows = screen.rows();
        assert_eq!(rows[0], TITLE[0].trim_end());
        assert_eq!(rows[9], "kept text");
        // The same size again changes nothing.
        let before = screen.sink.0.lock().unwrap().len();
        assert_eq!(frame.sync(TerminalSize::new(44, 120)), body);
        assert_eq!(screen.sink.0.lock().unwrap().len(), before);
    }

    #[test]
    fn a_small_terminal_gets_a_one_row_header() {
        let (mut screen, frame) = Screen::new(20, 80);
        frame.set_agent("agent-b", false);
        agent_writes(&frame, "body");
        let rows = screen.rows();
        assert!(rows[0].trim_start().starts_with("UULDANA"), "{:?}", rows[0]);
        assert!(rows[0].contains("agent-b"));
        assert_eq!(rows[1], "body");
        assert_eq!(frame.body_size(), TerminalSize::new(19, 80));
    }

    #[test]
    fn the_agent_and_the_news_show_in_the_header() {
        let (mut screen, frame) = Screen::new(40, 100);
        frame.set_agent("agent-a", false);
        frame.set_status("continuing on agent-b from agent-a");
        let rows = screen.rows();
        assert!(rows[6].contains("now: agent-a"), "{:?}", rows[6]);
        assert_eq!(rows[7], "continuing on agent-b from agent-a");
    }

    #[test]
    fn the_badge_stays_in_the_header() {
        let sink = Sink::default();
        let frame = Frame::start(
            Box::new(sink.clone()),
            TerminalSize::new(40, 100),
            false,
            Some("PERMISSIONS BYPASSED".into()),
        );
        frame.set_status("something else");
        let mut terminal = vt100::Parser::new(non_zero(40), non_zero(100), 0);
        terminal.process(&sink.0.lock().unwrap());
        let row = terminal
            .screen()
            .contents()
            .split('\n')
            .nth(6)
            .unwrap()
            .to_owned();
        assert!(row.contains("PERMISSIONS BYPASSED"), "{row:?}");
    }

    #[test]
    fn history_can_be_read_and_the_screen_comes_back() {
        let (mut screen, frame) = Screen::new(40, 100);
        let lines: String = (1..=80).map(|n| format!("line {n}\r\n")).collect();
        agent_writes(&frame, &lines);
        assert!(!screen.rows().iter().any(|row| row == "line 1"));
        frame.scroll(20);
        let rows = screen.rows();
        assert!(
            rows[7].contains("rows back in the history"),
            "{:?}",
            rows[7]
        );
        assert!(rows.iter().any(|row| row == "line 40"), "{rows:?}");
        assert!(frame.to_live());
        assert!(!frame.to_live());
        assert_eq!(screen.row(7), HINT);
    }

    #[test]
    fn the_agent_cursor_is_reported_in_its_own_terminal() {
        let (_screen, frame) = Screen::new(40, 100);
        assert_eq!(frame.cursor(), (1, 1));
        agent_writes(&frame, "abc\r\nde");
        assert_eq!(frame.cursor(), (2, 3));
    }

    #[test]
    fn the_terminal_is_given_back_once() {
        let (screen, frame) = Screen::new(40, 100);
        let guard = Restore(frame.clone());
        drop(guard);
        frame.finish();
        let sent = screen.sink.0.lock().unwrap().clone();
        let text = String::from_utf8_lossy(&sent);
        assert_eq!(text.matches("\x1b[?1049l").count(), 1);
        assert!(text.starts_with("\x1b[?1049h"));
    }

    #[test]
    fn the_agent_title_is_passed_on() {
        let (screen, frame) = Screen::new(40, 100);
        agent_writes(&frame, "\x1b]0;Some Agent\x07x");
        let sent = screen.sink.0.lock().unwrap().clone();
        assert!(String::from_utf8_lossy(&sent).contains("\x1b]0;Some Agent\x07"));
    }
}
