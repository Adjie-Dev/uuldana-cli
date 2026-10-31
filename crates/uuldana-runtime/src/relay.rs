//! Terminal relay for a native PTY run.
//!
//! The relay connects the user's terminal to the provider's own terminal UI:
//! user input goes to the PTY unchanged, PTY output goes to the user's terminal
//! unchanged, and the PTY follows the size of the terminal. It does not
//! interpret the provider's output, so the provider keeps its native interface.
//!
//! One exception is deliberate. Windows ConPTY asks its host for the cursor
//! position (`ESC [ 6 n`) when it starts. Forwarded to the user's terminal, the
//! answer comes back as a key event, not as bytes, and never reaches the PTY,
//! which then waits forever. The relay answers that query itself and does not
//! forward it.
//!
//! A second exception is opt-in: with [`RelayOptions::agent_command`] the relay
//! watches what the user types and, when a whole line is `/agent`, keeps that
//! line from the provider and ends with [`RelayEnd::AgentMenu`]. See
//! [`CommandWatch`] for what counts as "a whole line".

use std::{
    io::Write,
    sync::atomic::{AtomicBool, Ordering},
    time::{Duration, Instant},
};

use tokio::sync::{mpsc, oneshot};
use uuldana_core::{ExitInfo, TerminalSize};
use uuldana_transport::Frame;

use crate::{Error, Supervisor};

/// How often an idle relay checks for a terminal resize.
const RESIZE_POLL: Duration = Duration::from_millis(200);

const CURSOR_QUERY: &[u8] = b"\x1b[6n";

/// Source of the user's cursor position as `(row, column)`, both 1-based.
pub type CursorPosition<'a> = &'a mut dyn FnMut() -> (u16, u16);

/// Removes cursor position queries from a byte stream, including a query split
/// across two reads.
#[derive(Default)]
pub struct CursorQueryFilter {
    carry: Vec<u8>,
}

impl CursorQueryFilter {
    /// Returns the bytes to forward and how many queries were removed.
    pub fn feed(&mut self, bytes: &[u8]) -> (Vec<u8>, usize) {
        let mut data = std::mem::take(&mut self.carry);
        data.extend_from_slice(bytes);
        let mut out = Vec::with_capacity(data.len());
        let mut queries = 0;
        let mut at = 0;
        while at < data.len() {
            let rest = &data[at..];
            if rest.starts_with(CURSOR_QUERY) {
                queries += 1;
                at += CURSOR_QUERY.len();
            } else if CURSOR_QUERY.starts_with(rest) {
                // A prefix of the query at the very end: wait for more bytes.
                self.carry = rest.to_vec();
                break;
            } else {
                out.push(data[at]);
                at += 1;
            }
        }
        (out, queries)
    }
}

/// How a relay ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RelayEnd {
    /// The provider exited on its own, with this status when it was observed.
    ProviderExited(Option<ExitInfo>),
    /// The caller's `stop_when` asked to stop and the run was stopped.
    Stopped,
    /// The user typed `/agent`. The provider is still running and has had the
    /// typed line erased; the caller owns the keyboard until it relays again.
    AgentMenu,
}

/// What the relay does beyond forwarding bytes.
#[derive(Debug, Clone, Copy, Default)]
pub struct RelayOptions<'a> {
    /// Take the line `/agent` away from the provider and end the relay.
    pub agent_command: bool,
    /// Kept up to date with whether the provider has the terminal's alternate
    /// screen on, as its output says.
    pub alt_screen: Option<&'a AtomicBool>,
}

/// Escape sequences that turn the alternate screen on or off.
const ALT_SCREEN_SWITCHES: [(&[u8], bool); 6] = [
    (b"\x1b[?1049h", true),
    (b"\x1b[?1049l", false),
    (b"\x1b[?1047h", true),
    (b"\x1b[?1047l", false),
    (b"\x1b[?47h", true),
    (b"\x1b[?47l", false),
];
const ALT_SCREEN_LONGEST: usize = 8;

/// Passes the provider's output on and notes whether it is on the alternate
/// screen, including a switch split across two writes.
struct AltScreenTrack<'a, W> {
    inner: W,
    flag: Option<&'a AtomicBool>,
    carry: Vec<u8>,
}

impl<'a, W> AltScreenTrack<'a, W> {
    fn new(inner: W, flag: Option<&'a AtomicBool>) -> Self {
        Self {
            inner,
            flag,
            carry: Vec::new(),
        }
    }

    fn track(&mut self, bytes: &[u8]) {
        let Some(flag) = self.flag else {
            return;
        };
        let mut data = std::mem::take(&mut self.carry);
        data.extend_from_slice(bytes);
        let last = ALT_SCREEN_SWITCHES
            .iter()
            .filter_map(|&(sequence, on)| {
                data.windows(sequence.len())
                    .rposition(|window| window == sequence)
                    .map(|at| (at, on))
            })
            .max_by_key(|&(at, _)| at);
        if let Some((_, on)) = last {
            flag.store(on, Ordering::Relaxed);
        }
        let keep = data.len().min(ALT_SCREEN_LONGEST - 1);
        self.carry = data[data.len() - keep..].to_vec();
    }
}

impl<W: Write> Write for AltScreenTrack<'_, W> {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        let written = self.inner.write(buf)?;
        self.track(&buf[..written]);
        Ok(written)
    }

    fn write_all(&mut self, buf: &[u8]) -> std::io::Result<()> {
        self.track(buf);
        self.inner.write_all(buf)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.inner.flush()
    }
}

/// The line the user is typing, as far as it can be known from the bytes sent.
///
/// The provider owns the input box, so this is a guess that must never be
/// wrong in the dangerous direction: anything it cannot follow (cursor keys,
/// tab, pasted escape sequences, long lines) makes it give up until the next
/// Enter. Only a line that is exactly `/agent`, with optional trailing spaces,
/// counts as the command.
#[derive(Debug, Default)]
pub struct CommandWatch {
    line: Vec<u8>,
    lost: bool,
    /// Inside a bracketed paste that has not ended yet.
    in_paste: bool,
}

/// What [`CommandWatch::feed`] decided about one read of keyboard bytes.
#[derive(Debug, PartialEq, Eq)]
pub struct Watched {
    /// Bytes for the provider.
    pub forward: Vec<u8>,
    /// The line was the command. Any bytes after its Enter were dropped.
    pub command: bool,
}

/// Whether `bytes` are exactly one mouse report: the SGR form
/// `ESC [ < b ; x ; y M` (or `m`), or the old form `ESC [ M` and three bytes.
fn is_mouse_report(bytes: &[u8]) -> bool {
    if let Some(inner) = bytes.strip_prefix(b"\x1b[<")
        && let Some((last, numbers)) = inner.split_last()
    {
        return matches!(last, b'M' | b'm')
            && !numbers.is_empty()
            && numbers.iter().all(|b| b.is_ascii_digit() || *b == b';');
    }
    bytes.len() == 6 && bytes.starts_with(b"\x1b[M")
}

/// Longest line worth following.
const WATCH_LIMIT: usize = 64;
const ERASE: u8 = 0x7f;
const PASTE_START: &[u8] = b"\x1b[200~";
const PASTE_END: &[u8] = b"\x1b[201~";

impl CommandWatch {
    /// Follows one read of keyboard bytes.
    pub fn feed(&mut self, bytes: &[u8]) -> Watched {
        // Pasted text goes to the provider whole and is never the command,
        // whatever it says; what is in the input box is no longer known.
        if self.in_paste || bytes.starts_with(PASTE_START) {
            let last = |marker: &[u8]| bytes.windows(marker.len()).rposition(|w| w == marker);
            self.in_paste = match (last(PASTE_START), last(PASTE_END)) {
                (Some(start), Some(end)) => start > end,
                (Some(_), None) => true,
                (None, Some(_)) => false,
                (None, None) => self.in_paste,
            };
            self.line.clear();
            self.lost = true;
            return Watched {
                forward: bytes.to_vec(),
                command: false,
            };
        }
        // A mouse event is not typing. The wheel turning between the letters
        // of `/agent` and the Enter must not make the line unrecognizable.
        if is_mouse_report(bytes) {
            return Watched {
                forward: bytes.to_vec(),
                command: false,
            };
        }
        let mut forward = Vec::with_capacity(bytes.len());
        for &byte in bytes {
            match byte {
                b'\r' | b'\n' => {
                    let command = !self.lost && self.line.trim_ascii_end() == b"/agent";
                    if command {
                        // The provider has seen the typed characters; remove
                        // them so it is left with an empty input box.
                        forward.extend(std::iter::repeat_n(ERASE, self.line.len()));
                        self.line.clear();
                        return Watched {
                            forward,
                            command: true,
                        };
                    }
                    self.line.clear();
                    self.lost = false;
                    forward.push(byte);
                }
                ERASE | 0x08 => {
                    self.line.pop();
                    forward.push(byte);
                }
                // Ctrl+C and Ctrl+U empty the line in the providers.
                0x03 | 0x15 => {
                    self.line.clear();
                    self.lost = false;
                    forward.push(byte);
                }
                0x20..=0x7e | 0x80..=0xff => {
                    if self.line.len() < WATCH_LIMIT {
                        self.line.push(byte);
                    } else {
                        self.lost = true;
                    }
                    forward.push(byte);
                }
                // Escape sequences, tab and the other control keys move or
                // rewrite the line in ways that are not followed here.
                _ => {
                    self.lost = true;
                    forward.push(byte);
                }
            }
        }
        Watched {
            forward,
            command: false,
        }
    }
}

/// Relays a started native PTY run until the provider exits or `stop_when`
/// asks to stop it.
///
/// `input` carries raw bytes read from the user's terminal. `output` receives
/// the provider's terminal output. `terminal_size` reports the current size of
/// the user's terminal; a change is applied to the PTY. When `cursor` is given,
/// the relay answers the pseudo terminal's own cursor queries (see the module
/// documentation); pass it on Windows. `stop_when` is asked after every frame
/// and on every idle tick, with the time since the provider last wrote output;
/// when it returns true the run is stopped gracefully.
///
/// The input receiver is handed back, so the same keyboard stream can continue
/// into the next run when the caller switches providers. Input writes run on
/// their own task, so a provider that is busy writing output never blocks the
/// user's keystrokes.
pub async fn relay<W: Write>(
    runtime: &mut Supervisor,
    input: mpsc::UnboundedReceiver<Vec<u8>>,
    output: W,
    terminal_size: impl FnMut() -> Option<TerminalSize>,
    cursor: Option<CursorPosition<'_>>,
    stop_when: impl FnMut(&Supervisor, Duration) -> bool,
) -> (
    Result<RelayEnd, Error>,
    Option<mpsc::UnboundedReceiver<Vec<u8>>>,
) {
    relay_with(
        RelayOptions::default(),
        runtime,
        input,
        output,
        terminal_size,
        cursor,
        stop_when,
    )
    .await
}

/// [`relay`] with options.
pub async fn relay_with<W: Write>(
    options: RelayOptions<'_>,
    runtime: &mut Supervisor,
    mut input: mpsc::UnboundedReceiver<Vec<u8>>,
    output: W,
    mut terminal_size: impl FnMut() -> Option<TerminalSize>,
    cursor: Option<CursorPosition<'_>>,
    mut stop_when: impl FnMut(&Supervisor, Duration) -> bool,
) -> (
    Result<RelayEnd, Error>,
    Option<mpsc::UnboundedReceiver<Vec<u8>>>,
) {
    let Some(handle) = runtime.input_handle() else {
        return (Err(Error::WorkerStopped), Some(input));
    };
    let (stop_tx, mut stop_rx) = oneshot::channel::<()>();
    let (command_tx, mut command_rx) = mpsc::unbounded_channel::<()>();
    let forwarder = {
        let handle = handle.clone();
        tokio::spawn(async move {
            let mut watch = options.agent_command.then(CommandWatch::default);
            loop {
                tokio::select! {
                    biased;
                    _ = &mut stop_rx => break,
                    bytes = input.recv() => match bytes {
                        Some(bytes) => {
                            let (forward, command) = match watch.as_mut() {
                                Some(watch) => {
                                    let watched = watch.feed(&bytes);
                                    (watched.forward, watched.command)
                                }
                                None => (bytes, false),
                            };
                            if !forward.is_empty() && handle.write(&forward).await.is_err() {
                                break;
                            }
                            if command {
                                let _ = command_tx.send(());
                                break;
                            }
                        }
                        None => break,
                    },
                }
            }
            input
        })
    };
    let mut output = AltScreenTrack::new(output, options.alt_screen);
    let result = pump(
        runtime,
        &mut output,
        &mut terminal_size,
        cursor,
        handle,
        &mut stop_when,
        &mut command_rx,
    )
    .await;
    let _ = stop_tx.send(());
    (result, forwarder.await.ok())
}

async fn pump<W: Write>(
    runtime: &mut Supervisor,
    output: &mut W,
    terminal_size: &mut impl FnMut() -> Option<TerminalSize>,
    mut cursor: Option<CursorPosition<'_>>,
    handle: uuldana_transport::InputHandle,
    stop_when: &mut impl FnMut(&Supervisor, Duration) -> bool,
    command: &mut mpsc::UnboundedReceiver<()>,
) -> Result<RelayEnd, Error> {
    let mut last = terminal_size();
    let mut exit = None;
    let mut filter = CursorQueryFilter::default();
    let mut last_output = Instant::now();
    loop {
        // Dropping an idle next_frame is safe: it only waits on channels.
        let waited = tokio::select! {
            biased;
            Some(()) = command.recv() => return Ok(RelayEnd::AgentMenu),
            waited = tokio::time::timeout(RESIZE_POLL, runtime.next_frame()) => waited,
        };
        let frame = match waited {
            Ok(frame) => frame?,
            Err(_) => {
                apply_resize(runtime, terminal_size, &mut last);
                if stop_when(runtime, last_output.elapsed()) {
                    let _ = runtime.stop().await;
                    return Ok(RelayEnd::Stopped);
                }
                continue;
            }
        };
        match frame {
            Some(Frame::Output(bytes)) | Some(Frame::Stderr(bytes)) => {
                last_output = Instant::now();
                let (forward, queries) = if cursor.is_some() {
                    filter.feed(&bytes)
                } else {
                    (bytes, 0)
                };
                if let Some(position) = cursor.as_mut() {
                    for _ in 0..queries {
                        let (row, col) = position();
                        // A failed reply is not fatal; the exit is reported by
                        // the transport.
                        let _ = handle.write(format!("[{row};{col}R").as_bytes()).await;
                    }
                }
                output
                    .write_all(&forward)
                    .and_then(|()| output.flush())
                    .map_err(|e| Error::Transport(uuldana_transport::Error::Io(e)))?;
            }
            Some(Frame::Exited(info)) => exit = Some(info),
            Some(_) => {}
            None => return Ok(RelayEnd::ProviderExited(exit)),
        }
        apply_resize(runtime, terminal_size, &mut last);
        if stop_when(runtime, last_output.elapsed()) {
            let _ = runtime.stop().await;
            return Ok(RelayEnd::Stopped);
        }
    }
}

fn apply_resize(
    runtime: &mut Supervisor,
    terminal_size: &mut impl FnMut() -> Option<TerminalSize>,
    last: &mut Option<TerminalSize>,
) {
    let current = terminal_size();
    // An invalid or unsupported size is skipped; the PTY keeps its size.
    if let Some(size) = current
        && current != *last
        && runtime.resize(size).is_ok()
    {
        *last = current;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn removes_queries_and_keeps_everything_else() {
        let mut filter = CursorQueryFilter::default();
        let (out, n) = filter.feed(b"\x1b[6nhello\x1b[31mred\x1b[6n");
        assert_eq!(out, b"hello\x1b[31mred");
        assert_eq!(n, 2);
    }

    #[test]
    fn handles_a_query_split_across_reads() {
        let mut filter = CursorQueryFilter::default();
        let (first, n1) = filter.feed(b"ab\x1b[");
        let (second, n2) = filter.feed(b"6ncd");
        assert_eq!((first.as_slice(), n1), (&b"ab"[..], 0));
        assert_eq!((second.as_slice(), n2), (&b"cd"[..], 1));
    }

    #[test]
    fn a_dangling_prefix_that_never_completes_is_not_lost() {
        let mut filter = CursorQueryFilter::default();
        let (first, _) = filter.feed(b"x\x1b[");
        let (second, n) = filter.feed(b"31m");
        assert_eq!(first, b"x");
        assert_eq!(second, b"\x1b[31m");
        assert_eq!(n, 0);
    }

    fn track(chunks: &[&[u8]]) -> (bool, Vec<u8>) {
        let flag = AtomicBool::new(false);
        let mut out = Vec::new();
        let mut tracked = AltScreenTrack::new(&mut out, Some(&flag));
        for chunk in chunks {
            tracked.write_all(chunk).unwrap();
        }
        drop(tracked);
        (flag.load(Ordering::Relaxed), out)
    }

    #[test]
    fn the_alternate_screen_is_followed_and_the_output_is_untouched() {
        let (on, out) = track(&[b"hello \x1b[?1049h\x1b[2Jscreen"]);
        assert!(on);
        assert_eq!(out, b"hello \x1b[?1049h\x1b[2Jscreen");
        assert!(!track(&[b"\x1b[?1049h", b"\x1b[?1049l"]).0);
        // The last switch in a read wins, whatever the order of the modes.
        assert!(track(&[b"\x1b[?1049l\x1b[?47h"]).0);
        assert!(!track(&[b"\x1b[?1049h\x1b[?1047l"]).0);
        assert!(!track(&[b"plain text and \x1b[31mcolor"]).0);
    }

    #[test]
    fn an_alternate_screen_switch_split_across_writes_is_found() {
        assert!(track(&[b"abc\x1b[?10", b"49hdef"]).0);
        assert!(!track(&[b"\x1b[?1049h", b"x\x1b", b"[?1049", b"l"]).0);
    }

    #[test]
    fn without_a_flag_the_output_passes_through() {
        let mut out = Vec::new();
        let mut tracked = AltScreenTrack::new(&mut out, None);
        tracked.write_all(b"\x1b[?1049hx").unwrap();
        drop(tracked);
        assert_eq!(out, b"\x1b[?1049hx");
    }

    fn typed(text: &str) -> Vec<Vec<u8>> {
        text.bytes().map(|byte| vec![byte]).collect()
    }

    /// Feeds one key per read and returns what the provider received and
    /// whether the command was taken.
    fn run_keys(keys: &[Vec<u8>]) -> (Vec<u8>, bool) {
        let mut watch = CommandWatch::default();
        let mut sent = Vec::new();
        for key in keys {
            let watched = watch.feed(key);
            sent.extend(watched.forward);
            if watched.command {
                return (sent, true);
            }
        }
        (sent, false)
    }

    #[test]
    fn a_whole_line_agent_is_taken_and_erased_from_the_provider() {
        let mut keys = typed("/agent");
        keys.push(b"\r".to_vec());
        let (sent, command) = run_keys(&keys);
        assert!(command);
        // The provider saw the six characters and then six erasures, never Enter.
        assert_eq!(sent, [&b"/agent"[..], &[ERASE; 6][..]].concat());
    }

    #[test]
    fn trailing_spaces_still_count_and_are_erased_too() {
        let mut keys = typed("/agent  ");
        keys.push(b"\n".to_vec());
        let (sent, command) = run_keys(&keys);
        assert!(command);
        assert!(sent.ends_with(&[ERASE; 8]));
    }

    #[test]
    fn other_lines_pass_through_untouched() {
        for text in [
            "/agents",
            "/agent x",
            "hello /agent",
            "agent",
            "/ agent",
            "/Agent",
        ] {
            let mut keys = typed(text);
            keys.push(b"\r".to_vec());
            let (sent, command) = run_keys(&keys);
            assert!(!command, "{text}");
            assert_eq!(sent, [text.as_bytes(), b"\r"].concat(), "{text}");
        }
    }

    #[test]
    fn editing_the_line_is_followed() {
        // A typo fixed with backspace still ends up as the command.
        let mut keys = typed("/agex");
        keys.push(vec![ERASE]);
        keys.extend(typed("nt"));
        keys.push(b"\r".to_vec());
        assert!(run_keys(&keys).1);
        // Ctrl+U clears, so what came before it does not count.
        let mut keys = typed("note ");
        keys.push(vec![0x15]);
        keys.extend(typed("/agent"));
        keys.push(b"\r".to_vec());
        assert!(run_keys(&keys).1);
        // Backspacing past the start leaves the line empty, not negative.
        let mut keys = vec![vec![ERASE]; 3];
        keys.extend(typed("/agent"));
        keys.push(b"\r".to_vec());
        assert!(run_keys(&keys).1);
    }

    #[test]
    fn a_line_the_relay_cannot_follow_is_never_taken() {
        // Cursor keys, tab and other control keys mean the line is unknown.
        for key in [&b"\x1b[D"[..], b"\t", b"\x01", b"\x1b"] {
            let mut keys = typed("/agent");
            keys.push(key.to_vec());
            keys.push(b"\r".to_vec());
            assert!(!run_keys(&keys).1, "{key:?}");
        }
        // A very long line is passed on whole and does not confuse the next one.
        let long = "x".repeat(WATCH_LIMIT + 5);
        let mut keys = typed(&long);
        keys.push(b"\r".to_vec());
        let (sent, command) = run_keys(&keys);
        assert!(!command);
        assert_eq!(sent, [long.as_bytes(), b"\r"].concat());
        keys.extend(typed("/agent"));
        keys.push(b"\r".to_vec());
        assert!(run_keys(&keys).1);
    }

    #[test]
    fn a_mouse_event_in_the_middle_of_the_line_does_not_stop_the_command() {
        // The wheel turns between the letters and the Enter.
        for report in [&b"\x1b[<64;5;12M"[..], b"\x1b[<65;120;40M", b"\x1b[M`,'"] {
            let mut keys = typed("/ag");
            keys.push(report.to_vec());
            keys.extend(typed("ent"));
            keys.push(report.to_vec());
            keys.push(b"\r".to_vec());
            let (sent, command) = run_keys(&keys);
            assert!(command, "{report:?}");
            // The agent saw the reports (it asked for them) and the letters.
            assert!(sent.starts_with(b"/ag"), "{sent:?}");
        }
        // Things that only look like one are still control input.
        for not_a_report in [&b"\x1b[<64;5;12X"[..], b"\x1b[<M", b"\x1b[<a;1;1M"] {
            let mut keys = typed("/agent");
            keys.push(not_a_report.to_vec());
            keys.push(b"\r".to_vec());
            assert!(!run_keys(&keys).1, "{not_a_report:?}");
        }
    }

    #[test]
    fn the_next_line_starts_clean_after_enter_or_ctrl_c() {
        let mut keys = typed("/agent");
        keys.push(b"\x1b[D".to_vec());
        keys.push(b"\r".to_vec());
        keys.extend(typed("/agent"));
        keys.push(b"\r".to_vec());
        assert!(run_keys(&keys).1);
        let mut keys = typed("/agent");
        keys.push(b"\x01".to_vec());
        keys.push(vec![0x03]);
        keys.extend(typed("/agent"));
        keys.push(b"\r".to_vec());
        assert!(run_keys(&keys).1);
    }

    #[test]
    fn a_pasted_command_is_text_and_not_taken() {
        let mut keys = vec![b"\x1b[200~/agent\x1b[201~".to_vec()];
        keys.push(b"\r".to_vec());
        let (sent, command) = run_keys(&keys);
        assert!(!command);
        assert_eq!(sent, [&b"\x1b[200~/agent\x1b[201~"[..], b"\r"].concat());
        // A paste that arrives in pieces, with lines inside it, is followed to
        // its end and the next line starts clean.
        let mut keys = vec![b"\x1b[200~first\n".to_vec(), b"/agent\n".to_vec()];
        keys.push(b"x\x1b[201~".to_vec());
        keys.push(b"\r".to_vec());
        keys.extend(typed("/agent"));
        keys.push(b"\r".to_vec());
        assert!(run_keys(&keys).1);
    }

    #[test]
    fn one_read_with_the_whole_command_is_handled_and_the_rest_is_dropped() {
        let watched = CommandWatch::default().feed(b"/agent\rignored");
        assert!(watched.command);
        assert_eq!(watched.forward, [&b"/agent"[..], &[ERASE; 6][..]].concat());
    }
}
