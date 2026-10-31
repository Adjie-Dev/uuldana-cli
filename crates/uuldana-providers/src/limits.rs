//! Usage-limit phrases from a provider manifest.
//!
//! A manifest may list texts its program prints when the usage limit is
//! reached. They are compared after control sequences are removed and
//! whitespace is collapsed, and a match is labeled inferred, because terminal
//! text is not a documented interface. A program whose manifest lists none is
//! covered by the general reader in the runtime.

use uuldana_provider_sdk::{
    AgentEvent, DecodedEvent, EventDecoder, LimitKind, LimitSignal, TransportFrame,
};

/// Rolling window of normalized text kept between chunks.
const WINDOW: usize = 4096;

/// Strips terminal control sequences, lowercases and collapses whitespace.
/// Cursor movement becomes a space, because full-screen UIs position words with
/// it instead of printing spaces.
pub(crate) fn normalize(bytes: &[u8]) -> String {
    let text = String::from_utf8_lossy(bytes);
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\x1b' => match chars.peek() {
                Some('[') => {
                    chars.next();
                    let mut last = ' ';
                    for next in chars.by_ref() {
                        if ('\u{40}'..='\u{7e}').contains(&next) {
                            last = next;
                            break;
                        }
                    }
                    if matches!(last, 'A'..='H' | 'f' | 'd' | 'e' | 'a' | '`') {
                        out.push(' ');
                    }
                }
                Some(']') => {
                    chars.next();
                    while let Some(next) = chars.next() {
                        if next == '\x07' || (next == '\x1b' && chars.peek() == Some(&'\\')) {
                            if next == '\x1b' {
                                chars.next();
                            }
                            break;
                        }
                    }
                }
                Some(_) => {
                    chars.next();
                }
                None => {}
            },
            c if c.is_control() => out.push(' '),
            c => out.push(c),
        }
    }
    let mut collapsed = String::with_capacity(out.len());
    let mut space = false;
    for c in out.chars() {
        if c.is_whitespace() || c == '\u{2219}' || c == '\u{b7}' || c == '\u{2022}' {
            space = true;
        } else {
            if space {
                collapsed.push(' ');
            }
            space = false;
            collapsed.extend(c.to_lowercase());
        }
    }
    // Edge spaces are kept as one space so words split across chunks are not
    // glued together, and words separated by a chunk boundary stay separate.
    if space {
        collapsed.push(' ');
    }
    collapsed.replace('\u{2019}', "'")
}

/// Decoder for providers described by a manifest: any of the given phrases,
/// compared after normalization, is a usage-limit stop.
pub(crate) struct PhraseLimitDecoder {
    phrases: Vec<String>,
    window: String,
}

impl PhraseLimitDecoder {
    pub(crate) fn new(phrases: &[String]) -> Self {
        Self {
            phrases: phrases
                .iter()
                .map(|p| normalize(p.as_bytes()).trim().to_owned())
                .filter(|p| !p.is_empty())
                .collect(),
            window: String::new(),
        }
    }
}

impl EventDecoder for PhraseLimitDecoder {
    fn decode(&mut self, frame: TransportFrame<'_>, out: &mut Vec<DecodedEvent>) {
        let TransportFrame::Output(bytes) = frame else {
            return;
        };
        let text = normalize(bytes);
        let text = if self.window.ends_with(' ') {
            text.trim_start_matches(' ')
        } else {
            &text
        };
        self.window.push_str(text);
        let hit = self
            .phrases
            .iter()
            .filter_map(|p| self.window.find(p.as_str()).map(|at| (at, p.len())))
            .min();
        if let Some((at, len)) = hit {
            out.push(DecodedEvent::inferred(AgentEvent::LimitReached(
                LimitSignal {
                    kind: LimitKind::UsageQuota,
                    resets_at: None,
                    message: Some(self.window[at..at + len].to_owned()),
                },
            )));
            self.window = self.window[at + len..].to_owned();
        }
        if self.window.len() > WINDOW {
            let mut cut = self.window.len() - WINDOW;
            while !self.window.is_char_boundary(cut) {
                cut += 1;
            }
            self.window = self.window[cut..].to_owned();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(phrases: &[&str], chunks: &[&[u8]]) -> Vec<LimitSignal> {
        let phrases: Vec<String> = phrases.iter().map(|p| (*p).to_owned()).collect();
        let mut decoder = PhraseLimitDecoder::new(&phrases);
        let mut out = Vec::new();
        for chunk in chunks {
            decoder.decode(TransportFrame::Output(chunk), &mut out);
        }
        out.into_iter()
            .map(|decoded| {
                assert_eq!(decoded.fidelity, uuldana_provider_sdk::Fidelity::Inferred);
                match decoded.event {
                    AgentEvent::LimitReached(signal) => signal,
                    other => panic!("unexpected {other:?}"),
                }
            })
            .collect()
    }

    #[test]
    fn a_listed_phrase_is_found_through_decoration_and_split_chunks() {
        let styled = "\x1b[2K\x1b[1;31mQuota\x1b[0m\x1b[1Cexhausted\r\n";
        assert_eq!(run(&["quota exhausted"], &[styled.as_bytes()]).len(), 1);
        let signals = run(&["quota exhausted"], &[b"quota exha", b"usted for today"]);
        assert_eq!(signals.len(), 1);
        assert_eq!(signals[0].kind, LimitKind::UsageQuota);
    }

    #[test]
    fn ordinary_output_and_other_frames_are_ignored() {
        assert!(run(&["quota exhausted"], &[b"Compiling a crate"]).is_empty());
        let phrases = vec!["quota exhausted".to_owned()];
        let mut decoder = PhraseLimitDecoder::new(&phrases);
        let mut out = Vec::new();
        decoder.decode(TransportFrame::Stderr(b"quota exhausted"), &mut out);
        decoder.decode(TransportFrame::Message("quota exhausted"), &mut out);
        assert!(out.is_empty());
    }

    #[test]
    fn one_message_fires_once_but_a_later_one_fires_again() {
        let phrases = vec!["quota exhausted".to_owned()];
        let mut decoder = PhraseLimitDecoder::new(&phrases);
        let mut out = Vec::new();
        decoder.decode(TransportFrame::Output(b"quota exhausted"), &mut out);
        decoder.decode(TransportFrame::Output(b"other output"), &mut out);
        assert_eq!(out.len(), 1);
        decoder.decode(TransportFrame::Output(b"quota exhausted again"), &mut out);
        assert_eq!(out.len(), 2);
    }
}
