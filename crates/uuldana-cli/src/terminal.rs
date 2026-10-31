//! The user's terminal: raw mode, key encoding and size.
//!
//! A provider's native UI expects the bytes a terminal emulator sends. On a
//! real console the user's keys arrive as key events, so they are encoded here
//! into the usual xterm sequences. The bytes are then relayed unchanged.

use std::time::Duration;

use crossterm::event::{
    DisableMouseCapture, EnableMouseCapture, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers,
    MouseEventKind,
};
use tokio::sync::mpsc;
use uuldana_core::TerminalSize;

/// One notch of the mouse wheel and where the pointer was, 1-based, in the
/// terminal.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Wheel {
    /// Away from the user.
    pub up: bool,
    /// Column of the pointer.
    pub column: u16,
    /// Row of the pointer.
    pub row: u16,
}

/// Puts the terminal into raw mode and restores it on drop, including when the
/// session ends with an error or a panic.
///
/// The mouse is captured as well. On the alternate screen a terminal turns the
/// wheel into arrow keys, which an agent takes for its own input history; with
/// the mouse captured the wheel reaches Uuldana, which scrolls the screen's
/// history. Selecting text then takes Shift held down.
pub struct RawMode;

impl RawMode {
    /// Enables raw mode.
    pub fn enable() -> std::io::Result<Self> {
        crossterm::terminal::enable_raw_mode()?;
        let _ = crossterm::execute!(std::io::stdout(), EnableMouseCapture);
        Ok(Self)
    }
}

impl Drop for RawMode {
    fn drop(&mut self) {
        let _ = crossterm::execute!(std::io::stdout(), DisableMouseCapture);
        let _ = crossterm::terminal::disable_raw_mode();
    }
}

/// The bytes that stand for a mouse event on their way to the relay: only the
/// wheel counts. They are not a sequence any key sends, so nothing typed can be
/// mistaken for them; [`parse_wheel`] reads them back.
pub fn wheel_bytes(kind: MouseEventKind, column: u16, row: u16) -> Option<Vec<u8>> {
    let code = match kind {
        MouseEventKind::ScrollUp => 5,
        MouseEventKind::ScrollDown => 6,
        _ => return None,
    };
    Some(
        format!(
            "\x1b[{code};99;{};{}~",
            column.saturating_add(1),
            row.saturating_add(1)
        )
        .into_bytes(),
    )
}

/// The wheel notch that `bytes` stand for, if they are one.
pub fn parse_wheel(bytes: &[u8]) -> Option<Wheel> {
    let text = std::str::from_utf8(bytes).ok()?;
    let inner = text.strip_prefix("\x1b[")?.strip_suffix('~')?;
    let mut parts = inner.split(';');
    let up = match parts.next()? {
        "5" => true,
        "6" => false,
        _ => return None,
    };
    if parts.next()? != "99" {
        return None;
    }
    let column = parts.next()?.parse().ok()?;
    let row = parts.next()?.parse().ok()?;
    parts.next().is_none().then_some(Wheel { up, column, row })
}

/// What an agent that asked for the mouse is sent for a wheel notch, in the
/// encoding it asked for. `column` and `row` are 1-based within its terminal.
pub fn mouse_report(sgr: bool, up: bool, column: u16, row: u16) -> Vec<u8> {
    // Button 4 and 5 of the xterm numbering are the wheel.
    let button: u16 = if up { 64 } else { 65 };
    if sgr {
        format!("\x1b[<{button};{column};{row}M").into_bytes()
    } else {
        // The old encoding has one byte per number and no room past 223.
        let byte = |value: u16| u8::try_from(value.min(223) + 32).unwrap_or(255);
        vec![0x1b, b'[', b'M', byte(button), byte(column), byte(row)]
    }
}

/// Current terminal size, if the terminal reports one.
pub fn size() -> Option<TerminalSize> {
    let (cols, rows) = crossterm::terminal::size().ok()?;
    let size = TerminalSize::new(rows, cols);
    size.is_valid().then_some(size)
}

const PASTE_START: &[u8] = b"\x1b[200~";
const PASTE_END: &[u8] = b"\x1b[201~";

/// Characters read together at least this many at a time are a paste, not
/// typing. A person does not produce four keys before the reader looks again.
const PASTE_MIN: usize = 4;
/// A paste with a line break in it is recognized from this many characters.
const PASTE_MIN_WITH_BREAK: usize = 3;
/// Text is held back this long to see whether it is the start of a paste.
/// Nobody types two characters closer together than this.
const PASTE_PROBE: Duration = Duration::from_millis(20);
/// A paste goes on while the next piece is no later than this.
const PASTE_GAP: Duration = Duration::from_millis(60);

/// `text` as a bracketed paste, the way a terminal sends it.
pub fn paste_bytes(text: &str) -> Vec<u8> {
    let mut bytes = PASTE_START.to_vec();
    bytes.extend_from_slice(text.as_bytes());
    bytes.extend_from_slice(PASTE_END);
    bytes
}

/// `bytes` for an agent that did not ask for bracketed pastes: the markers are
/// taken out and the line feeds of a paste become returns, as if typed.
/// `inside` says whether a paste is open, and carries over to the next read.
pub fn unbracket(bytes: &[u8], inside: &mut bool) -> Vec<u8> {
    let mut out = Vec::with_capacity(bytes.len());
    let mut at = 0;
    while at < bytes.len() {
        let rest = &bytes[at..];
        if rest.starts_with(PASTE_START) {
            *inside = true;
            at += PASTE_START.len();
        } else if rest.starts_with(PASTE_END) {
            *inside = false;
            at += PASTE_END.len();
        } else {
            out.push(if *inside && bytes[at] == b'\n' {
                b'\r'
            } else {
                bytes[at]
            });
            at += 1;
        }
    }
    out
}

/// The character a key press adds to a paste: text, a line break or a tab,
/// with no modifier other than Shift.
fn paste_char(event: &Event) -> Option<char> {
    let Event::Key(key) = event else {
        return None;
    };
    if key.kind == KeyEventKind::Release
        || key
            .modifiers
            .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT)
    {
        return None;
    }
    match key.code {
        KeyCode::Char(c) => Some(c),
        KeyCode::Enter => Some('\n'),
        KeyCode::Tab => Some('\t'),
        _ => None,
    }
}

/// Whether events that were all waiting at once are a paste.
///
/// The Windows console has no bracketed paste: the terminal types the pasted
/// text into the console, line breaks included, and each one would send the
/// message. Text that arrives faster than anyone types is told apart by how
/// much of it is there at once.
fn is_paste_burst(events: &[Event]) -> bool {
    let chars = events.iter().filter_map(paste_char);
    let (count, has_break) = chars.fold((0, false), |(count, has_break), c| {
        (count + 1, has_break || c == '\n')
    });
    count >= PASTE_MIN || (has_break && count >= PASTE_MIN_WITH_BREAK)
}

/// The bytes for `events` read as one paste: the text runs become bracketed
/// pastes, and anything else in between (a key that is not text) goes out as
/// it normally would, in its place.
fn encode_paste(events: Vec<Event>) -> Vec<Vec<u8>> {
    let mut out = Vec::new();
    let mut text = String::new();
    for event in events {
        if let Some(c) = paste_char(&event) {
            text.push(c);
            continue;
        }
        if let Event::Key(key) = &event
            && key.kind == KeyEventKind::Release
        {
            continue;
        }
        if !text.is_empty() {
            out.push(paste_bytes(&std::mem::take(&mut text)));
        }
        out.extend(encode_event(event));
    }
    if !text.is_empty() {
        out.push(paste_bytes(&text));
    }
    out
}

fn encode_event(event: Event) -> Option<Vec<u8>> {
    match event {
        Event::Key(key) => encode_key(key),
        Event::Paste(text) => Some(paste_bytes(&text)),
        Event::Mouse(mouse) => wheel_bytes(mouse.kind, mouse.column, mouse.row),
        _ => None,
    }
}

/// Whether the clipboard holds a picture and no text, which is what Ctrl+V
/// means to an agent: look for an image.
#[cfg(windows)]
fn clipboard_has_only_image() -> bool {
    use clipboard_win::{formats, is_format_avail};
    let image = [formats::CF_DIB, formats::CF_DIBV5, formats::CF_BITMAP]
        .into_iter()
        .any(is_format_avail);
    image && !is_format_avail(formats::CF_UNICODETEXT) && !is_format_avail(formats::CF_TEXT)
}

#[cfg(not(windows))]
fn clipboard_has_only_image() -> bool {
    false
}

/// Turns Ctrl+V with a picture on the clipboard into the key that attaches it.
///
/// Windows Terminal takes Ctrl+V for its own paste. With a picture on the
/// clipboard there is no text to paste, and only the release of the key gets
/// through, so the agent never hears of it. Claude Code on Windows attaches a
/// picture on Alt+V and not on Ctrl+V; Codex takes both. So Ctrl+V, pressed or
/// only released, becomes Alt+V while the clipboard holds a picture and no
/// text. With text on the clipboard the terminal has pasted it already and
/// nothing is added.
#[derive(Default)]
struct PictureKey {
    held: bool,
}

impl PictureKey {
    fn filter(&mut self, event: Event, has_only_image: impl Fn() -> bool) -> Event {
        let Event::Key(mut key) = event else {
            return event;
        };
        if key.code != KeyCode::Char('v') || key.modifiers != KeyModifiers::CONTROL {
            return Event::Key(key);
        }
        let attach = match key.kind {
            KeyEventKind::Release => !std::mem::take(&mut self.held) && has_only_image(),
            _ => {
                self.held = true;
                has_only_image()
            }
        };
        if attach {
            key.modifiers = KeyModifiers::ALT;
            key.kind = KeyEventKind::Press;
        }
        Event::Key(key)
    }
}

/// The text that `events` type.
fn paste_text(events: &[Event]) -> String {
    events.iter().filter_map(paste_char).collect()
}

fn holds_text(events: &[Event]) -> bool {
    events.iter().any(|event| paste_char(event).is_some())
}

/// Reads the events that are there, or that arrive within `wait`, onto
/// `events`. False when there were none.
fn read_more(events: &mut Vec<Event>, wait: Duration) -> bool {
    let mut any = false;
    let mut wait = wait;
    while crossterm::event::poll(wait).unwrap_or(false) {
        match crossterm::event::read() {
            Ok(event) => events.push(event),
            Err(_) => break,
        }
        any = true;
        wait = Duration::ZERO;
    }
    any
}

/// The files that sit on the clipboard next to a picture, as the Snipping Tool
/// leaves them. Empty when there is no picture or no file.
#[cfg(windows)]
fn clipboard_picture_files() -> Vec<String> {
    use clipboard_win::{formats, get_clipboard, is_format_avail};
    let picture = [formats::CF_DIB, formats::CF_DIBV5, formats::CF_BITMAP]
        .into_iter()
        .any(is_format_avail);
    if !picture {
        return Vec::new();
    }
    get_clipboard::<Vec<String>, _>(formats::FileList).unwrap_or_default()
}

#[cfg(not(windows))]
fn clipboard_picture_files() -> Vec<String> {
    Vec::new()
}

/// Whether `text` is made of the paths of `files`.
fn is_path_of(files: &[String], text: &str) -> bool {
    let text = text.to_lowercase();
    !files.is_empty() && files.iter().all(|file| text.contains(&file.to_lowercase()))
}

/// A paste on its way to the agent, a piece at a time. The marker that opens
/// it goes out with the first text, so the agent starts on a long paste while
/// the rest is still coming in, and the marker that closes it goes out when
/// no more comes.
#[derive(Default)]
struct PasteStream {
    inside: bool,
}

impl PasteStream {
    /// The bytes for the next `events` of the paste. A key that is not text
    /// ends the paste there and goes out as it normally would.
    fn piece(&mut self, events: Vec<Event>, picture: &mut PictureKey) -> Vec<Vec<u8>> {
        let mut out = Vec::new();
        let mut text = String::new();
        for event in events {
            let event = picture.filter(event, clipboard_has_only_image);
            if let Some(c) = paste_char(&event) {
                text.push(c);
                continue;
            }
            if let Event::Key(key) = &event
                && key.kind == KeyEventKind::Release
            {
                continue;
            }
            self.flush(&mut text, &mut out);
            self.close(&mut out);
            out.extend(encode_event(event));
        }
        self.flush(&mut text, &mut out);
        out
    }

    /// The marker that closes the paste, if it is open.
    fn finish(&mut self) -> Vec<Vec<u8>> {
        let mut out = Vec::new();
        self.close(&mut out);
        out
    }

    fn flush(&mut self, text: &mut String, out: &mut Vec<Vec<u8>>) {
        if text.is_empty() {
            return;
        }
        if !self.inside {
            self.inside = true;
            out.push(PASTE_START.to_vec());
        }
        out.push(std::mem::take(text).into_bytes());
    }

    fn close(&mut self, out: &mut Vec<Vec<u8>>) {
        if self.inside {
            self.inside = false;
            out.push(PASTE_END.to_vec());
        }
    }
}

/// Reads key events on a background thread and sends their bytes. A paste is
/// sent as one bracketed paste, a piece at a time (see [`is_paste_burst`]).
/// The thread ends when the receiver is dropped or the terminal input fails.
pub fn spawn_input() -> mpsc::UnboundedReceiver<Vec<u8>> {
    let (tx, rx) = mpsc::unbounded_channel();
    std::thread::Builder::new()
        .name("uuldana-keys".into())
        .spawn(move || {
            let mut picture = PictureKey::default();
            while let Ok(first) = crossterm::event::read() {
                let mut events = vec![first];
                read_more(&mut events, Duration::ZERO);
                // The first characters of a paste can come alone, ahead of the
                // rest. Text is given a moment to show whether more follows,
                // until there is enough of it to be sure.
                while holds_text(&events)
                    && !is_paste_burst(&events)
                    && read_more(&mut events, PASTE_PROBE)
                {}
                let paste = is_paste_burst(&events);
                // A picture on the clipboard that came with its file: the
                // terminal pastes the file's path, and the picture is attached
                // on its own, so the path is not wanted. Only the whole paste
                // tells whether it is that path, so it is read to its end first.
                let files = if paste {
                    clipboard_picture_files()
                } else {
                    Vec::new()
                };
                let chunks: Vec<Vec<u8>> = if paste && files.is_empty() {
                    let mut stream = PasteStream::default();
                    let mut batch = events;
                    loop {
                        for bytes in stream.piece(batch, &mut picture) {
                            if tx.send(bytes).is_err() {
                                return;
                            }
                        }
                        batch = Vec::new();
                        // A long paste comes in pieces; wait for the rest.
                        if !read_more(&mut batch, PASTE_GAP) {
                            break;
                        }
                    }
                    stream.finish()
                } else if paste {
                    while read_more(&mut events, PASTE_GAP) {}
                    let skip_text = is_path_of(&files, &paste_text(&events));
                    let events: Vec<Event> = events
                        .into_iter()
                        .filter(|event| !(skip_text && paste_char(event).is_some()))
                        .map(|event| picture.filter(event, clipboard_has_only_image))
                        .collect();
                    if skip_text {
                        events.into_iter().filter_map(encode_event).collect()
                    } else {
                        encode_paste(events)
                    }
                } else {
                    events
                        .into_iter()
                        .map(|event| picture.filter(event, clipboard_has_only_image))
                        .filter_map(encode_event)
                        .collect()
                };
                for bytes in chunks {
                    if tx.send(bytes).is_err() {
                        return;
                    }
                }
            }
        })
        .expect("spawn key reader");
    rx
}

/// What stands for Shift+Enter or Ctrl+Enter on its way to `route`: a private
/// sequence no key sends.
const LINE_BREAK_MARKER: &[u8] = b"\x1b[13;99~";

/// Whether `bytes` are the marker for a line break key.
pub fn is_line_break(bytes: &[u8]) -> bool {
    bytes == LINE_BREAK_MARKER
}

/// A line break for an agent whose console takes keys as the Windows console
/// does (`ESC [ ? 9001 h`): the key Ctrl+J, pressed and released. A console
/// reads it as a key, with Ctrl on it, so an agent that reads the console's keys
/// (Codex) takes it for a new line, and the console turns it into a line feed
/// for an agent that reads text (Claude Code), which takes that for a new line
/// too. Shift+Enter cannot be sent as itself: the console makes it a plain
/// Enter for an agent that reads text, and that sends what was typed.
pub const WIN32_LINE_BREAK: &[u8] = b"\x1b[74;36;10;1;8;1_\x1b[74;36;10;0;8;1_";

/// A line break for an agent whose console does not take keys that way: a line
/// feed, which is what Ctrl+J sends.
pub const LINE_FEED: &[u8] = b"\n";

/// Bytes a terminal emulator would send for `key`. Release events and keys
/// with no terminal encoding produce nothing.
pub fn encode_key(key: KeyEvent) -> Option<Vec<u8>> {
    if key.kind == KeyEventKind::Release {
        return None;
    }
    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
    let alt = key.modifiers.contains(KeyModifiers::ALT);
    let shift = key.modifiers.contains(KeyModifiers::SHIFT);
    // xterm modifier parameter: 1 + shift(1) + alt(2) + ctrl(4).
    let modifier = 1 + u8::from(shift) + 2 * u8::from(alt) + 4 * u8::from(ctrl);
    let csi = |letter: char| -> Vec<u8> {
        if modifier == 1 {
            format!("\x1b[{letter}").into_bytes()
        } else {
            format!("\x1b[1;{modifier}{letter}").into_bytes()
        }
    };
    let tilde = |number: u8| -> Vec<u8> {
        if modifier == 1 {
            format!("\x1b[{number}~").into_bytes()
        } else {
            format!("\x1b[{number};{modifier}~").into_bytes()
        }
    };
    let mut bytes = match key.code {
        KeyCode::Char(c) => {
            let mut out = Vec::new();
            if ctrl {
                out.push(control_byte(c)?);
            } else {
                let mut buf = [0u8; 4];
                out.extend_from_slice(c.encode_utf8(&mut buf).as_bytes());
            }
            out
        }
        // Shift+Enter and Ctrl+Enter add a line instead of sending. A console
        // gives an agent no way to tell them from Enter, but a line feed is
        // what Ctrl+J sends, and every agent takes that as a new line.
        // Shift+Enter and Ctrl+Enter add a line instead of sending. What an
        // agent takes for that depends on how its console is fed, which only
        // the screen knows, so they leave here as a marker for `route`.
        KeyCode::Enter if (shift || ctrl) && !alt => LINE_BREAK_MARKER.to_vec(),
        KeyCode::Enter => vec![b'\r'],
        KeyCode::Tab => vec![b'\t'],
        KeyCode::BackTab => b"\x1b[Z".to_vec(),
        // Ctrl+Backspace deletes a word. Agents know that as Ctrl+W.
        KeyCode::Backspace if ctrl && !alt => vec![0x17],
        KeyCode::Backspace => vec![0x7f],
        KeyCode::Esc => vec![0x1b],
        KeyCode::Up => csi('A'),
        KeyCode::Down => csi('B'),
        KeyCode::Right => csi('C'),
        KeyCode::Left => csi('D'),
        KeyCode::Home => csi('H'),
        KeyCode::End => csi('F'),
        KeyCode::Insert => tilde(2),
        KeyCode::Delete => tilde(3),
        KeyCode::PageUp => tilde(5),
        KeyCode::PageDown => tilde(6),
        KeyCode::F(n @ 1..=4) => {
            format!("\x1bO{}", ["P", "Q", "R", "S"][usize::from(n) - 1]).into_bytes()
        }
        KeyCode::F(n @ 5..=12) => tilde([15, 17, 18, 19, 20, 21, 23, 24][usize::from(n) - 5]),
        _ => return None,
    };
    // Alt prefixes plain characters with ESC. Arrow and tilde keys already
    // carry it in their modifier parameter.
    if alt
        && matches!(
            key.code,
            KeyCode::Char(_) | KeyCode::Enter | KeyCode::Backspace
        )
    {
        bytes.insert(0, 0x1b);
    }
    Some(bytes)
}

fn control_byte(c: char) -> Option<u8> {
    match c {
        'a'..='z' => Some(c as u8 - b'a' + 1),
        'A'..='Z' => Some(c as u8 - b'A' + 1),
        '@' | ' ' | '2' => Some(0),
        '[' | '3' => Some(0x1b),
        '\\' | '4' => Some(0x1c),
        ']' | '5' => Some(0x1d),
        '^' | '6' => Some(0x1e),
        '_' | '7' | '/' => Some(0x1f),
        '8' => Some(0x7f),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::KeyEventState;

    fn key(code: KeyCode, modifiers: KeyModifiers) -> KeyEvent {
        KeyEvent {
            code,
            modifiers,
            kind: KeyEventKind::Press,
            state: KeyEventState::NONE,
        }
    }

    fn encode(code: KeyCode, modifiers: KeyModifiers) -> Vec<u8> {
        encode_key(key(code, modifiers)).expect("encodable")
    }

    #[test]
    fn text_and_unicode_pass_through() {
        assert_eq!(encode(KeyCode::Char('a'), KeyModifiers::NONE), b"a");
        assert_eq!(encode(KeyCode::Char('A'), KeyModifiers::SHIFT), b"A");
        assert_eq!(
            encode(KeyCode::Char('é'), KeyModifiers::NONE),
            "é".as_bytes()
        );
    }

    #[test]
    fn control_keys_become_control_bytes() {
        assert_eq!(encode(KeyCode::Char('c'), KeyModifiers::CONTROL), [0x03]);
        assert_eq!(encode(KeyCode::Char('d'), KeyModifiers::CONTROL), [0x04]);
        assert_eq!(encode(KeyCode::Char('z'), KeyModifiers::CONTROL), [0x1a]);
        assert_eq!(encode(KeyCode::Char('['), KeyModifiers::CONTROL), [0x1b]);
        assert!(encode_key(key(KeyCode::Char('!'), KeyModifiers::CONTROL)).is_none());
    }

    #[test]
    fn editing_and_navigation_keys_use_xterm_sequences() {
        assert_eq!(encode(KeyCode::Enter, KeyModifiers::NONE), b"\r");
        assert_eq!(encode(KeyCode::Backspace, KeyModifiers::NONE), [0x7f]);
        assert_eq!(encode(KeyCode::Esc, KeyModifiers::NONE), [0x1b]);
        assert_eq!(encode(KeyCode::Up, KeyModifiers::NONE), b"\x1b[A");
        assert_eq!(encode(KeyCode::Left, KeyModifiers::NONE), b"\x1b[D");
        assert_eq!(encode(KeyCode::Delete, KeyModifiers::NONE), b"\x1b[3~");
        assert_eq!(encode(KeyCode::PageDown, KeyModifiers::NONE), b"\x1b[6~");
        assert_eq!(encode(KeyCode::BackTab, KeyModifiers::SHIFT), b"\x1b[Z");
        assert_eq!(encode(KeyCode::F(1), KeyModifiers::NONE), b"\x1bOP");
        assert_eq!(encode(KeyCode::F(5), KeyModifiers::NONE), b"\x1b[15~");
        assert_eq!(encode(KeyCode::F(12), KeyModifiers::NONE), b"\x1b[24~");
    }

    #[test]
    fn modifiers_follow_the_xterm_parameter() {
        assert_eq!(encode(KeyCode::Up, KeyModifiers::SHIFT), b"\x1b[1;2A");
        assert_eq!(encode(KeyCode::Right, KeyModifiers::CONTROL), b"\x1b[1;5C");
        assert_eq!(encode(KeyCode::Left, KeyModifiers::ALT), b"\x1b[1;3D");
        assert_eq!(encode(KeyCode::Delete, KeyModifiers::CONTROL), b"\x1b[3;5~");
        assert_eq!(encode(KeyCode::Char('b'), KeyModifiers::ALT), b"\x1bb");
    }

    #[test]
    fn only_the_wheel_of_the_mouse_is_passed_on() {
        let up = wheel_bytes(MouseEventKind::ScrollUp, 4, 20).unwrap();
        let down = wheel_bytes(MouseEventKind::ScrollDown, 0, 0).unwrap();
        assert_eq!(
            parse_wheel(&up),
            Some(Wheel {
                up: true,
                column: 5,
                row: 21
            })
        );
        assert_eq!(
            parse_wheel(&down),
            Some(Wheel {
                up: false,
                column: 1,
                row: 1
            })
        );
        assert_eq!(wheel_bytes(MouseEventKind::Moved, 1, 1), None);
        // No key produces what the wheel stands for, and ordinary text is not a wheel.
        for code in [
            KeyCode::PageUp,
            KeyCode::PageDown,
            KeyCode::Up,
            KeyCode::Down,
        ] {
            for modifiers in [
                KeyModifiers::NONE,
                KeyModifiers::SHIFT,
                KeyModifiers::CONTROL,
                KeyModifiers::ALT,
            ] {
                assert_eq!(parse_wheel(&encode(code, modifiers)), None);
            }
        }
        assert_eq!(parse_wheel(b"a"), None);
        assert_eq!(parse_wheel(b"\x1b[5;99;1~"), None);
    }

    #[test]
    fn a_wheel_notch_is_reported_to_an_agent_the_way_it_asked() {
        assert_eq!(mouse_report(true, true, 12, 7), b"\x1b[<64;12;7M");
        assert_eq!(mouse_report(true, false, 12, 7), b"\x1b[<65;12;7M");
        // The old encoding: one byte per number, offset by 32, and no further
        // than 223.
        assert_eq!(
            mouse_report(false, true, 12, 7),
            [0x1b, b'[', b'M', 96, 44, 39]
        );
        assert_eq!(mouse_report(false, false, 500, 500)[4..], [255, 255]);
    }

    fn typed_text(text: &str) -> Vec<Event> {
        text.chars()
            .map(|c| {
                let code = if c == '\n' {
                    KeyCode::Enter
                } else {
                    KeyCode::Char(c)
                };
                Event::Key(key(code, KeyModifiers::NONE))
            })
            .collect()
    }

    #[test]
    fn shift_and_ctrl_enter_add_a_line_and_plain_enter_sends() {
        assert_eq!(encode(KeyCode::Enter, KeyModifiers::NONE), b"\r");
        for modifiers in [
            KeyModifiers::SHIFT,
            KeyModifiers::CONTROL,
            KeyModifiers::CONTROL | KeyModifiers::SHIFT,
        ] {
            assert!(is_line_break(&encode(KeyCode::Enter, modifiers)));
        }
        assert_eq!(encode(KeyCode::Enter, KeyModifiers::ALT), b"\x1b\r");
        // Only the marker is one, and no ordinary key sends it.
        assert!(!is_line_break(b"\r"));
        assert!(!is_line_break(b"\n"));
        assert!(!is_line_break(b"\x1b[13;2~"));
    }

    #[test]
    fn a_line_break_is_ctrl_j_in_the_way_each_console_takes_it() {
        // A Windows console is given the key, pressed and released with Ctrl;
        // any other, the line feed that Ctrl+J is. Never Enter with Shift,
        // which a console makes a plain Enter that sends the message.
        assert_eq!(
            WIN32_LINE_BREAK,
            b"\x1b[74;36;10;1;8;1_\x1b[74;36;10;0;8;1_"
        );
        assert_eq!(LINE_FEED, b"\n");
        assert!(!String::from_utf8_lossy(WIN32_LINE_BREAK).contains("[13;"));
    }

    #[test]
    fn ctrl_backspace_deletes_a_word() {
        assert_eq!(encode(KeyCode::Backspace, KeyModifiers::CONTROL), [0x17]);
        assert_eq!(encode(KeyCode::Backspace, KeyModifiers::NONE), [0x7f]);
        assert_eq!(encode(KeyCode::Backspace, KeyModifiers::ALT), b"\x1b\x7f");
    }

    #[test]
    fn text_that_arrives_at_once_is_a_paste() {
        assert!(is_paste_burst(&typed_text("hello world")));
        assert!(is_paste_burst(&typed_text("a\nb")));
        // Typing, however fast, is never four keys at once.
        assert!(!is_paste_burst(&typed_text("abc")));
        assert!(!is_paste_burst(&typed_text("a\n")));
        assert!(!is_paste_burst(&[Event::Key(key(
            KeyCode::Enter,
            KeyModifiers::NONE
        ))]));
        // Keys with Ctrl or Alt are shortcuts, not text.
        let shortcuts: Vec<Event> = "abcd"
            .chars()
            .map(|c| Event::Key(key(KeyCode::Char(c), KeyModifiers::CONTROL)))
            .collect();
        assert!(!is_paste_burst(&shortcuts));
    }

    #[test]
    fn a_paste_keeps_its_line_breaks_inside_one_bracketed_paste() {
        let chunks = encode_paste(typed_text("first line\nsecond line\n"));
        assert_eq!(chunks, [paste_bytes("first line\nsecond line\n")]);
    }

    #[test]
    fn a_key_in_the_middle_of_a_paste_stays_a_key() {
        let mut events = typed_text("abcd");
        events.push(Event::Key(key(KeyCode::Left, KeyModifiers::NONE)));
        events.extend(typed_text("efgh"));
        assert_eq!(
            encode_paste(events),
            [paste_bytes("abcd"), b"\x1b[D".to_vec(), paste_bytes("efgh")]
        );
    }

    fn ctrl_v(kind: KeyEventKind) -> Event {
        let mut event = key(KeyCode::Char('v'), KeyModifiers::CONTROL);
        event.kind = kind;
        Event::Key(event)
    }

    fn sent(event: Event) -> Option<Vec<u8>> {
        encode_event(event)
    }

    #[test]
    fn a_lone_ctrl_v_release_with_a_picture_attaches_it() {
        let mut picture = PictureKey::default();
        let event = picture.filter(ctrl_v(KeyEventKind::Release), || true);
        assert_eq!(sent(event), Some(b"\x1bv".to_vec()));
    }

    #[test]
    fn ctrl_v_with_text_on_the_clipboard_adds_nothing() {
        let mut picture = PictureKey::default();
        let event = picture.filter(ctrl_v(KeyEventKind::Release), || false);
        assert_eq!(sent(event), None);
    }

    #[test]
    fn a_ctrl_v_that_arrives_whole_attaches_once() {
        let mut picture = PictureKey::default();
        let press = picture.filter(ctrl_v(KeyEventKind::Press), || true);
        let release = picture.filter(ctrl_v(KeyEventKind::Release), || true);
        assert_eq!(sent(press), Some(b"\x1bv".to_vec()));
        assert_eq!(sent(release), None);
        // The next one, lone this time, is told apart again.
        let again = picture.filter(ctrl_v(KeyEventKind::Release), || true);
        assert_eq!(sent(again), Some(b"\x1bv".to_vec()));
    }

    #[test]
    fn other_released_keys_stay_released() {
        let mut picture = PictureKey::default();
        for (code, modifiers) in [
            (KeyCode::Char('c'), KeyModifiers::CONTROL),
            (
                KeyCode::Char('v'),
                KeyModifiers::CONTROL | KeyModifiers::SHIFT,
            ),
            (KeyCode::Char('v'), KeyModifiers::NONE),
        ] {
            let mut released = key(code, modifiers);
            released.kind = KeyEventKind::Release;
            let event = picture.filter(Event::Key(released), || true);
            assert_eq!(sent(event), None, "{code:?} {modifiers:?}");
        }
    }

    #[test]
    fn the_text_of_events_leaves_out_keys_that_are_not_text() {
        let mut events = typed_text("ab\ncd");
        events.insert(2, Event::Key(key(KeyCode::Left, KeyModifiers::NONE)));
        events.push(ctrl_v(KeyEventKind::Release));
        assert!(holds_text(&events));
        assert_eq!(paste_text(&events), "ab\ncd");
        assert!(!holds_text(&[ctrl_v(KeyEventKind::Release)]));
    }

    #[test]
    fn an_agent_without_bracketed_pastes_gets_the_text_as_typed() {
        let mut inside = false;
        assert_eq!(unbracket(b"plain\n", &mut inside), b"plain\n");
        assert_eq!(unbracket(&paste_bytes("a\nb"), &mut inside), b"a\rb");
        assert!(!inside);
        // A paste that comes in pieces is followed from one read to the next.
        assert_eq!(unbracket(b"\x1b[200~one\n", &mut inside), b"one\r");
        assert!(inside);
        assert_eq!(unbracket(b"two\n", &mut inside), b"two\r");
        assert_eq!(unbracket(b"\x1b[201~\n", &mut inside), b"\n");
        assert!(!inside);
    }

    fn streamed(batches: Vec<Vec<Event>>) -> Vec<Vec<u8>> {
        let mut picture = PictureKey::default();
        let mut stream = PasteStream::default();
        let mut out = Vec::new();
        for batch in batches {
            out.extend(stream.piece(batch, &mut picture));
        }
        out.extend(stream.finish());
        out
    }

    #[test]
    fn a_paste_goes_out_a_piece_at_a_time_inside_one_bracket() {
        let out = streamed(vec![typed_text("first\nsec"), typed_text("ond\n")]);
        assert_eq!(
            out,
            [
                PASTE_START.to_vec(),
                b"first\nsec".to_vec(),
                b"ond\n".to_vec(),
                PASTE_END.to_vec()
            ]
        );
        let mut inside = false;
        let text: Vec<u8> = out.iter().flat_map(|c| unbracket(c, &mut inside)).collect();
        assert_eq!(text, b"first\rsecond\r");
    }

    #[test]
    fn a_key_in_a_streamed_paste_closes_it_and_the_text_after_opens_it_again() {
        let mut events = typed_text("abcd");
        events.push(Event::Key(key(KeyCode::Left, KeyModifiers::NONE)));
        events.extend(typed_text("efgh"));
        assert_eq!(
            streamed(vec![events]),
            [
                PASTE_START.to_vec(),
                b"abcd".to_vec(),
                PASTE_END.to_vec(),
                b"\x1b[D".to_vec(),
                PASTE_START.to_vec(),
                b"efgh".to_vec(),
                PASTE_END.to_vec()
            ]
        );
        assert!(streamed(vec![Vec::new()]).is_empty());
    }

    #[test]
    fn a_path_is_told_from_text_by_the_files_on_the_clipboard() {
        let files = vec![r"C:\Users\a\ScreenClip\{ABC}.png".to_owned()];
        assert!(is_path_of(&files, r"c:\users\a\screenclip\{abc}.png"));
        assert!(is_path_of(&files, r#""C:\Users\a\ScreenClip\{ABC}.png""#));
        assert!(!is_path_of(&files, "some other text"));
        assert!(!is_path_of(&[], "anything"));
    }

    #[test]
    fn releases_and_unmapped_keys_produce_nothing() {
        let mut release = key(KeyCode::Char('a'), KeyModifiers::NONE);
        release.kind = KeyEventKind::Release;
        assert!(encode_key(release).is_none());
        assert!(encode_key(key(KeyCode::CapsLock, KeyModifiers::NONE)).is_none());
        assert!(encode_key(key(KeyCode::F(30), KeyModifiers::NONE)).is_none());
    }
}
