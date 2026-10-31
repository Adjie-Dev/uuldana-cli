//! Usage-limit detection for any agent, read from its terminal text.
//!
//! An adapter that knows its provider's wording supplies its own decoder. For
//! every other native terminal agent this one is used, so a provider added
//! tomorrow can fail over without any code written for it.
//!
//! The text is not a documented interface and differs by provider, version and
//! plan, so no list of sentences is kept. A stop is read by its grammar: a word
//! that says an allowance is gone (hit, reached, exhausted, out of) next to a
//! word that names it (limit, quota, credits), or an error code that says the
//! same. The stretch is then scored by what stands around it: when the allowance
//! returns or what to do, a time, being addressed, being short. A warning about
//! an allowance only partly used, a limit per minute, a question, code, or a
//! limit of something else is turned down. Every match is labeled inferred, and
//! a stretch that looked like a stop and was turned down is reported as a
//! diagnostic, so a stop that was missed can be seen afterwards.

use uuldana_provider_sdk::{
    AgentEvent, DecodedEvent, EventDecoder, LimitKind, LimitSignal, TransportFrame,
};

/// Rolling window of normalized text kept between chunks.
const WINDOW: usize = 4096;
/// Text kept before a phrase and after it when looking for the rest of the
/// message.
const BEFORE: usize = 120;
const AFTER: usize = 200;

/// Words that say an allowance is gone. A phrase is a run of words.
const STATE: &[&[&str]] = &[
    &["hit"],
    &["reached"],
    &["reach"],
    &["exceeded"],
    &["exceed"],
    &["exhausted"],
    &["depleted"],
    &["insufficient"],
    &["used", "up"],
    &["ran", "out"],
    &["run", "out"],
    &["out", "of"],
    &["maxed", "out"],
    &["no", "more"],
    &["no", "remaining"],
    &["too", "low"],
];

/// Words that name what the allowance is of.
const SUBJECT: &[&str] = &[
    "limit",
    "limits",
    "quota",
    "quotas",
    "allowance",
    "credit",
    "credits",
    "usage",
    "capacity",
    "balance",
];

/// Phrases that say the allowance is gone and name it at once.
const SELF_CONTAINED: &[&[&str]] = &[&["no", "quota"], &["no", "credits"], &["no", "credit"]];

/// Error codes that mean the same thing without any prose.
const ERROR_TOKENS: &[&str] = &[
    "insufficient_quota",
    "quota_exceeded",
    "resource_exhausted",
    "limit_reached",
];

/// Most words between the word that says the allowance is gone and the one
/// that names it ("hit your weekly limit", "limit has been reached").
const MAX_GAP: usize = 3;

/// Score a candidate needs to count as a stop. It is the lowest score that
/// every message in the corpus tests passes with room to spare, and that no
/// text in the negative corpus reaches.
pub const THRESHOLD: i32 = 3;

/// Words that say when the allowance returns or what to do about it. Whole
/// words only: "git reset" and "waiting" say nothing about an allowance.
const WHEN_OR_WHAT: &[&[&str]] = &[
    &["resets"],
    &["will", "reset"],
    &["reset", "at"],
    &["reset", "on"],
    &["reset", "in"],
    &["reset", "after"],
    &["reset", "by"],
    &["try", "again"],
    &["retry", "after"],
    &["upgrade"],
    &["add", "credits"],
    &["buy", "credits"],
    &["top", "up"],
    &["purchase"],
    &["renews"],
    &["renewal"],
    &["available", "again"],
    &["billing"],
    &["until"],
    &["wait"],
    &["later"],
    &["come", "back"],
    &["tomorrow"],
    &["next", "month"],
];

/// Wording that makes the text a pause or a note, not a stop: an allowance
/// that is only partly used ("approaching", "close to", "you've used"), and a
/// limit per minute, which passes. A share below 100% and a count that is left
/// are checked apart, see [`partly_used`].
const NOT_A_STOP: &[&[&str]] = &[
    &["you've", "used"],
    &["you", "have", "used"],
    &["approaching"],
    &["almost"],
    &["nearly"],
    &["close", "to"],
    &["running", "low"],
    &["per", "minute"],
    &["per", "second"],
    &["retrying", "in"],
    &["retry", "in"],
    &["retrying"],
    &["rate", "limit"],
    &["ratelimit"],
    &["rate-limit"],
    &["ratelimitexceeded"],
    &["continuing", "with", "usage", "credits"],
];

/// Wording about limits of something else than an allowance.
const OTHER_LIMITS: &[&[&str]] = &[
    &["context", "window"],
    &["context", "limit"],
    &["token", "limit"],
    &["memory", "limit"],
    &["time", "limit"],
    &["size", "limit"],
    &["character", "limit"],
    &["line", "limit"],
    &["output", "limit"],
    &["result", "limit"],
    &["response", "limit"],
    &["head_limit"],
    &["file", "size"],
    &["max", "tokens"],
];

/// Marks of source code: punctuation, and words that open a definition.
const CODE_MARKS: &[&str] = &[
    "();", "() {", " // ", "///", "//!", "=>", "};", "::", "(\"", "\")",
];
const CODE_WORDS: &[&str] = &["fn", "def", "function"];

/// Words between the name of the allowance and the word that says it is gone
/// that leave the sentence as terse as it was ("limit has been reached").
const AUXILIARY: &[&str] = &[
    "is", "was", "has", "have", "been", "being", "now", "already", "just",
];

/// Words that open a question, an instruction or a description of code, not a
/// message to the user.
const NOT_A_MESSAGE_START: &[&str] = &[
    "what",
    "how",
    "why",
    "if",
    "does",
    "do",
    "should",
    "can",
    "check",
    "ensure",
    "when",
    "whether",
    "to",
    "make",
    "handle",
    "detect",
    "test",
    "assert",
    "document",
    "describe",
    "explain",
    "write",
    "add",
    "implement",
    "fix",
    "show",
    "display",
    "log",
    "print",
    "return",
    "throw",
    "raise",
    "catch",
];

/// Words that address the reader.
const ADDRESSES: &[&str] = &["you", "your", "you've", "you're", "you'll", "you'd"];

const TIME_WORDS: &[&str] = &[
    "mon",
    "tue",
    "wed",
    "thu",
    "fri",
    "sat",
    "sun",
    "monday",
    "tuesday",
    "wednesday",
    "thursday",
    "friday",
    "saturday",
    "sunday",
    "jan",
    "feb",
    "apr",
    "jun",
    "jul",
    "aug",
    "sep",
    "oct",
    "nov",
    "dec",
    "january",
    "february",
    "april",
    "june",
    "july",
    "august",
    "september",
    "october",
    "november",
    "december",
    "tomorrow",
    "midnight",
    "tonight",
    "today",
    "am",
    "pm",
];

/// Strips terminal control sequences, lowercases and collapses whitespace.
/// Cursor movement becomes a space, because full-screen UIs position words with
/// it instead of printing spaces.
pub fn normalize(bytes: &[u8]) -> String {
    let text = String::from_utf8_lossy(bytes);
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    let mut row_seen: Option<String> = None;
    while let Some(c) = chars.next() {
        match c {
            '\x1b' => match chars.peek() {
                Some('[') => {
                    chars.next();
                    let mut last = ' ';
                    let mut params = String::new();
                    for next in chars.by_ref() {
                        if ('\u{40}'..='\u{7e}').contains(&next) {
                            last = next;
                            break;
                        }
                        params.push(next);
                    }
                    match last {
                        // Moving to a row: a new line unless it is the row
                        // the text is on already. Columns are only spaces.
                        'H' | 'f' => {
                            let row = params.split(';').next().unwrap_or("").to_owned();
                            out.push(if row_seen.as_deref() == Some(row.as_str()) {
                                ' '
                            } else {
                                '\n'
                            });
                            row_seen = Some(row);
                        }
                        'A' | 'B' | 'E' | 'F' | 'd' | 'e' => out.push('\n'),
                        'C' | 'D' | 'G' | 'a' | '`' => out.push(' '),
                        _ => {}
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
            '\n' => out.push('\n'),
            c if c.is_control() => out.push(' '),
            c => out.push(c),
        }
    }
    let mut collapsed = String::with_capacity(out.len());
    let mut space = false;
    let mut newline = false;
    for c in out.chars() {
        if c.is_whitespace() || c == '\u{2219}' || c == '\u{b7}' || c == '\u{2022}' {
            space = true;
            newline |= c == '\n';
        } else {
            if space {
                collapsed.push(if newline { '\n' } else { ' ' });
            }
            space = false;
            newline = false;
            collapsed.extend(c.to_lowercase());
        }
    }
    // Edge spaces are kept as one space so words split across chunks are not
    // glued together.
    if space {
        collapsed.push(if newline { '\n' } else { ' ' });
    }
    collapsed.replace('\u{2019}', "'")
}

fn boundary_down(text: &str, mut at: usize) -> usize {
    while !text.is_char_boundary(at) {
        at -= 1;
    }
    at
}

fn boundary_up(text: &str, mut at: usize) -> usize {
    while !text.is_char_boundary(at) {
        at += 1;
    }
    at
}

/// Where to cut `text` at or after `at`, so that what is left begins on a line:
/// just after the next line break, or at `at` when there is none. A cut in the
/// middle of a line takes away the start of it, and with it what says it is
/// source code or a note, so that the rest of it can read as a message.
fn cut_at_line(text: &str, at: usize) -> usize {
    let at = boundary_up(text, at.min(text.len()));
    text[at..].find('\n').map_or(at, |i| at + i + 1)
}

/// A stretch of text that may be a usage-limit stop, with how it was judged.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Candidate {
    /// Byte range of the stretch, from its first word to the end of what it is
    /// read with.
    pub start: usize,
    /// End of the range.
    pub end: usize,
    /// Evidence found, see [`THRESHOLD`].
    pub score: i32,
    /// Why it was turned down outright, when it was.
    pub rejected: Option<&'static str>,
}

impl Candidate {
    /// The candidate is a stop.
    pub fn is_stop(&self) -> bool {
        self.rejected.is_none() && self.score >= THRESHOLD
    }
}

/// How a stretch says the allowance is gone.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Kind {
    /// The word that names the allowance is not the first of the two.
    Plain,
    /// The name comes first and the state follows at once ("limit reached",
    /// "quota exceeded"), as a short line from the program words it.
    Terse,
    /// An error code.
    Token,
}

struct Word<'a> {
    text: &'a str,
    start: usize,
    end: usize,
}

fn words(text: &str) -> Vec<Word<'_>> {
    let mut out = Vec::new();
    let mut at = 0;
    for part in text.split([' ', '\n']) {
        let start = at;
        at += part.len() + 1;
        let trimmed = part.trim_matches(|c: char| !(c.is_alphanumeric() || c == '_' || c == '\''));
        if trimmed.is_empty() {
            continue;
        }
        let offset = part.find(trimmed).unwrap_or(0);
        out.push(Word {
            text: trimmed,
            start: start + offset,
            end: start + offset + trimmed.len(),
        });
    }
    out
}

fn matches_at(ws: &[Word<'_>], at: usize, phrase: &[&str]) -> bool {
    at + phrase.len() <= ws.len()
        && phrase
            .iter()
            .enumerate()
            .all(|(k, word)| ws[at + k].text == *word)
}

/// Every stretch of `text` (normalized) that says an allowance is gone, in
/// order, each judged.
pub fn candidates(text: &str) -> Vec<Candidate> {
    let ws = words(text);
    let mut spans: Vec<(usize, usize, Kind)> = Vec::new();
    for i in 0..ws.len() {
        for phrase in SELF_CONTAINED {
            if matches_at(&ws, i, phrase) {
                spans.push((ws[i].start, ws[i + phrase.len() - 1].end, Kind::Terse));
            }
        }
        for phrase in STATE {
            if !matches_at(&ws, i, phrase) {
                continue;
            }
            let last = i + phrase.len() - 1;
            let before = (i.saturating_sub(MAX_GAP + 1)..i)
                .rev()
                .find(|&j| SUBJECT.contains(&ws[j].text));
            let after = (last + 1..(last + 2 + MAX_GAP).min(ws.len()))
                .find(|&j| SUBJECT.contains(&ws[j].text));
            if let Some(j) = after {
                spans.push((ws[i].start, ws[j].end, Kind::Plain));
            } else if let Some(j) = before {
                // "limit reached", "limit has been reached": the program
                // words it as a terse line.
                let kind = if ws[j + 1..i].iter().all(|w| AUXILIARY.contains(&w.text)) {
                    Kind::Terse
                } else {
                    Kind::Plain
                };
                spans.push((ws[j].start, ws[last].end, kind));
            }
        }
    }
    for token in ERROR_TOKENS {
        let mut from = 0;
        while let Some(at) = text[from..].find(token).map(|i| i + from) {
            spans.push((at, at + token.len(), Kind::Token));
            from = at + token.len();
        }
    }
    spans.sort_unstable();
    spans.dedup();
    let mut out: Vec<Candidate> = Vec::new();
    for (start, end, kind) in spans {
        let candidate = judge(text, start, end, kind);
        // Overlapping stretches are one message; the best judged one stands.
        match out.last_mut() {
            Some(last) if start < last.end => {
                let better = match (candidate.is_stop(), last.is_stop()) {
                    (true, false) => true,
                    (false, true) => false,
                    _ => candidate.score > last.score,
                };
                if better {
                    *last = candidate;
                }
            }
            _ => out.push(candidate),
        }
    }
    out
}

fn has_phrase(ws: &[Word<'_>], phrases: &[&[&str]]) -> usize {
    phrases
        .iter()
        .filter(|phrase| (0..ws.len()).any(|at| matches_at(ws, at, phrase)))
        .count()
}

/// Whether `region` speaks of an allowance that is only partly used: a share
/// below 100% ("85% used", "only 10% remaining"), or a count that is left
/// ("10 messages left", "3 requests remaining"). Only an allowance that is
/// gone is a stop, whatever words are used for it.
fn partly_used(region: &str, ws: &[Word<'_>]) -> bool {
    let says_left = |after: &str| {
        words(after)
            .iter()
            .take(3)
            .any(|w| matches!(w.text, "left" | "remaining"))
    };
    for (at, byte) in region.bytes().enumerate() {
        if byte != b'%' {
            continue;
        }
        let digits: String = region[..at]
            .chars()
            .rev()
            .take_while(|c| c.is_ascii_digit() || *c == '.')
            .collect::<Vec<_>>()
            .into_iter()
            .rev()
            .collect();
        let Ok(share) = digits.parse::<f64>() else {
            continue;
        };
        // A share that is left is partly used when it is more than nothing;
        // a share that is used, when it is less than all.
        let partly = if says_left(&region[at + 1..]) {
            share > 0.0
        } else {
            share < 100.0
        };
        if partly {
            return true;
        }
    }
    (0..ws.len()).any(|i| {
        ws[i].text.parse::<u64>().is_ok_and(|count| count > 0)
            && (i + 1..(i + 3).min(ws.len())).any(|j| matches!(ws[j].text, "left" | "remaining"))
    })
}

/// Scores the stretch `start..end` of `text` by what stands around it. What is
/// looked at is the line the stretch begins on and up to two line breaks after
/// it (a long message wraps), not the rest of the screen.
fn judge(text: &str, start: usize, end: usize, kind: Kind) -> Candidate {
    let lo = boundary_down(text, start.saturating_sub(BEFORE));
    let hi = boundary_up(text, (end + AFTER).min(text.len()));
    let candidate = |score: i32, rejected: Option<&'static str>| Candidate {
        start,
        end: hi,
        score,
        rejected,
    };
    let from_line = text[..start].rfind('\n').map_or(0, |i| i + 1).max(lo);
    let to_line = {
        let mut breaks = 0;
        let mut to = hi;
        for (i, c) in text[end..hi].char_indices() {
            if c == '\n' {
                breaks += 1;
                if breaks == 2 {
                    to = end + i;
                    break;
                }
            }
        }
        to
    };
    let around = &text[from_line..to_line];
    let around_words = words(around);
    if has_phrase(&around_words, NOT_A_STOP) > 0 {
        return candidate(0, Some("a warning, a pause or a note"));
    }
    if partly_used(around, &around_words) {
        return candidate(0, Some("the allowance is only partly used"));
    }
    if has_phrase(&around_words, OTHER_LIMITS) > 0 {
        return candidate(0, Some("a limit of something else"));
    }
    // Source code is not a message: no message to the user has these in it.
    if CODE_MARKS.iter().any(|w| around.contains(w))
        || around_words.iter().any(|w| CODE_WORDS.contains(&w.text))
    {
        return candidate(0, Some("source code"));
    }
    // A line that is a string literal, in a list of messages or a translation
    // file, quotes a message; it is not one.
    let own_line = text[from_line..].split('\n').next().unwrap_or("").trim();
    if own_line.starts_with('"')
        && own_line
            .trim_end_matches([',', ';', ')', ']'])
            .ends_with('"')
    {
        return candidate(0, Some("a quoted string in source"));
    }
    let mut score = 0;
    let hints = has_phrase(&around_words, WHEN_OR_WHAT);
    if hints >= 1 {
        score += 2;
    }
    if hints >= 2 {
        score += 1;
    }
    if around_words.iter().any(|w| {
        TIME_WORDS.contains(&w.text)
            || (w.text.ends_with("am") || w.text.ends_with("pm"))
                && w.text.starts_with(|c: char| c.is_ascii_digit())
    }) {
        score += 1;
    }
    // The sentence the stretch is in.
    let boundary = |c: char| matches!(c, '.' | '!' | '?' | '\n');
    let from = text[..start]
        .rfind(boundary)
        .map(|i| i + 1)
        .filter(|&i| i >= from_line)
        .unwrap_or(from_line);
    let to = text[end..]
        .find(boundary)
        .map(|i| i + end)
        .filter(|&i| i <= hi)
        .unwrap_or(hi);
    let sentence = &text[from..to];
    let sentence_words = words(sentence);
    // A message to the user speaks to the reader, is an error code, or is
    // worded by the program as a terse line; prose about someone else's limit
    // is none of these. Being addressed is read close to the words, since the
    // text around is whatever else is on the screen.
    let near = {
        let before = words(&text[from_line..start]);
        let first = before.len().saturating_sub(6);
        let mut near: Vec<&str> = before[first..].iter().map(|w| w.text).collect();
        near.extend(words(&text[start..end]).iter().map(|w| w.text));
        near
    };
    let addressed = near.iter().any(|w| ADDRESSES.contains(w));
    let short = sentence_words.len() <= 8;
    if addressed {
        score += 1;
    }
    if short {
        score += 1;
    }
    match kind {
        // An error code is only a name: it stands in source and in logs as
        // often as in a message, so it counts for nothing without what goes
        // with a message.
        Kind::Token | Kind::Plain => {}
        Kind::Terse => score += 1,
    }
    if !(addressed || kind == Kind::Token || (kind == Kind::Terse && short)) {
        return candidate(score, Some("not worded as a message to the user"));
    }
    if sentence_words
        .first()
        .is_some_and(|w| NOT_A_MESSAGE_START.contains(&w.text))
    {
        score -= 3;
    }
    if sentence.contains('`') {
        score -= 3;
    }
    candidate(score, None)
}

/// Where a usage-limit stop is in `text` (normalized), as a byte range.
fn find_stop(text: &str) -> Option<(usize, usize)> {
    candidates(text)
        .into_iter()
        .find(Candidate::is_stop)
        .map(|c| (c.start, c.end))
}

/// `text` with the lines that say an allowance is gone taken out, so that an
/// agent given the text to carry on from does not show it back and look as if
/// it had hit a limit itself. A message that runs over two lines is found too.
pub fn redact_limit_text(text: &str) -> String {
    const MARKER: &str = "[usage limit message removed]";
    let lines: Vec<&str> = text.lines().collect();
    let normalized: Vec<String> = lines.iter().map(|l| normalize(l.as_bytes())).collect();
    let mut gone = vec![false; lines.len()];
    for i in 0..lines.len() {
        if find_stop(&normalized[i]).is_some() {
            gone[i] = true;
        } else if i + 1 < lines.len() {
            let pair = format!(
                "{} {}",
                normalized[i].trim_end(),
                normalized[i + 1].trim_start()
            );
            if find_stop(&pair).is_some() {
                gone[i] = true;
                gone[i + 1] = true;
            }
        }
    }
    let mut out = String::with_capacity(text.len());
    let mut was_gone = false;
    for (i, line) in lines.iter().enumerate() {
        if gone[i] {
            if !was_gone {
                out.push_str(MARKER);
                out.push('\n');
            }
        } else {
            out.push_str(line);
            out.push('\n');
        }
        was_gone = gone[i];
    }
    if !text.ends_with('\n') && out.ends_with('\n') {
        out.pop();
    }
    out
}

/// Decoder that reports a usage-limit stop found in terminal output. Text that
/// looks like one and is turned down is reported as a diagnostic.
#[derive(Default)]
pub struct UniversalLimitDecoder {
    window: String,
    diagnosed: Vec<String>,
}

/// Diagnostics one decoder reports at most.
const MAX_DIAGNOSTICS: usize = 20;
/// Longest snippet of text a diagnostic quotes.
const SNIPPET: usize = 160;

impl UniversalLimitDecoder {
    /// Creates a decoder with no text seen yet.
    pub fn new() -> Self {
        Self::default()
    }

    fn diagnose(&mut self, found: &[Candidate], out: &mut Vec<DecodedEvent>) {
        for candidate in found.iter().filter(|c| !c.is_stop()) {
            if self.diagnosed.len() >= MAX_DIAGNOSTICS {
                return;
            }
            let end = boundary_down(
                &self.window,
                candidate
                    .end
                    .min(candidate.start + SNIPPET)
                    .min(self.window.len()),
            );
            let snippet = self.window[candidate.start..end].trim().to_owned();
            if snippet.is_empty() || self.diagnosed.contains(&snippet) {
                continue;
            }
            let why = candidate.rejected.map_or_else(
                || format!("score {} is below {}", candidate.score, THRESHOLD),
                str::to_owned,
            );
            out.push(DecodedEvent::inferred(AgentEvent::Diagnostic {
                message: format!(
                    "text like a usage limit was not taken for one ({why}): {snippet}"
                ),
            }));
            self.diagnosed.push(snippet);
        }
    }
}

impl EventDecoder for UniversalLimitDecoder {
    fn decode(&mut self, frame: TransportFrame<'_>, out: &mut Vec<DecodedEvent>) {
        let TransportFrame::Output(bytes) = frame else {
            return;
        };
        let text = normalize(bytes);
        if text.is_empty() {
            return;
        }
        let text = if self.window.ends_with([' ', '\n']) {
            text.trim_start_matches([' ', '\n'])
        } else {
            &text
        };
        self.window.push_str(text);
        let found = candidates(&self.window);
        self.diagnose(&found, out);
        if let Some(stop) = found.iter().find(|c| c.is_stop()) {
            let message = self.window[stop.start..stop.end].trim().to_owned();
            out.push(DecodedEvent::inferred(AgentEvent::LimitReached(
                LimitSignal {
                    kind: LimitKind::UsageQuota,
                    resets_at: None,
                    message: Some(message),
                },
            )));
            // The same text must not fire again while it is still on screen.
            let cut = cut_at_line(&self.window, stop.end);
            self.window = self.window[cut..].to_owned();
        }
        if self.window.len() > WINDOW {
            let cut = cut_at_line(&self.window, self.window.len() - WINDOW);
            self.window = self.window[cut..].to_owned();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn signals(chunks: &[&[u8]]) -> Vec<LimitSignal> {
        let mut decoder = UniversalLimitDecoder::new();
        let mut out = Vec::new();
        for chunk in chunks {
            decoder.decode(TransportFrame::Output(chunk), &mut out);
        }
        out.into_iter()
            .filter_map(|decoded| match decoded.event {
                AgentEvent::LimitReached(signal) => Some(signal),
                _ => None,
            })
            .collect()
    }

    fn stops(text: &str) -> bool {
        !signals(&[text.as_bytes()]).is_empty()
    }

    /// The best score any candidate in `text` gets.
    fn best(text: &str) -> i32 {
        candidates(&normalize(text.as_bytes()))
            .iter()
            .filter(|c| c.rejected.is_none())
            .map(|c| c.score)
            .max()
            .unwrap_or(i32::MIN)
    }

    /// Messages an agent prints when the allowance is gone: the wording seen in
    /// public reports of Claude Code, Codex, Gemini CLI and Copilot, and the
    /// error text of the provider APIs. Weekly, session, 5-hour and per-model
    /// limits are the same to the reader.
    const STOPS: &[&str] = &[
        // Claude Code.
        "You've hit your weekly limit \u{b7} resets Mon 12:00am",
        "You've hit your session limit \u{b7} resets 3:45pm",
        "You've hit your Opus limit \u{b7} resets 3:45pm",
        "You've hit your Sonnet limit \u{b7} resets 3:45pm",
        "You've hit your limit \u{b7} resets Oct 31, 12am (Asia/Makassar)",
        "You've hit your 5-hour limit \u{b7} resets 3pm",
        "5-hour limit reached \u{2219} resets 3pm",
        "5-hour limit reached - resets 11:30 PM (Asia/Jakarta)",
        "Weekly limit reached \u{b7} resets Mon 9am",
        "Usage limit reached for all Pro models. Access resets at 3:40 PM.",
        "Claude usage limit reached. Your limit will reset at 3pm.",
        // Codex.
        "You've hit your usage limit. Upgrade to Pro (https://chatgpt.com/explore/pro), visit https://chatgpt.com/settings/usage to purchase more credits or try again at 7:11 PM.",
        "You've hit your usage limit. Upgrade to Plus to continue using Codex (https://chatgpt.com/explore/plus), or try again at Apr 21st, 2026 2:53 AM.",
        "You've hit your usage limit. Upgrade to Pro (https://chatgpt.com/explore/pro) or try again later.",
        "You've hit your usage limit. Try again in 8 hours 30 minutes.",
        "You've hit your usage limit. Your window resets at 2:47 PM.",
        "You've hit your usage limit. Upgrade your plan or add credits to continue, or try again at Nov 3rd, 2026 4:07 PM.",
        // Gemini CLI and Google APIs.
        "You have exhausted your capacity on this model. Your quota will reset after 3h 12m 5s.",
        "Error 429: RESOURCE_EXHAUSTED. Please try again later.",
        // Copilot.
        "Quota exceeded. Upgrade at https://github.com/features/copilot/plans",
        "You've exhausted your premium model quota. Enable additional paid premium requests, upgrade, or wait for renewal.",
        "You have exceeded your premium request allowance. Upgrade your plan or wait until it renews.",
        // Provider APIs.
        "Error: insufficient_quota. Add credits to your account to continue.",
        "You exceeded your current quota, please check your plan and billing details.",
        "Your credit balance is too low to access the API. Please go to Plans & Billing to upgrade or purchase credits.",
        // Others.
        "You've reached your monthly limit. Your limit resets on Nov 1.",
        "Daily limit reached. Try again tomorrow or upgrade your plan.",
        "You are out of credits. Top up to continue.",
        // Described by a provider's documentation, not quoted from it.
        "Monthly credit limit reached. Your plan resets on Nov 1.",
        // Wording the first general reader knew by heart, with what goes
        // with it, and the same said without addressing the reader.
        "Usage limit has been reached. Try again later.",
        "Your quota has been exhausted. Upgrade your plan to continue.",
        "Quota has been exhausted. Wait until Nov 1.",
        "The quota is exhausted. Try again tomorrow.",
        "No credits remaining. Top up to continue.",
        "You have run out of credits. Add credits to continue.",
        "You've reached your free usage limit. Upgrade to continue.",
        "You've hit the limit for today. Try again tomorrow.",
        "You have exceeded your limit. Upgrade your plan.",
        // Used up is used up, said in percent too.
        "You've reached 100% of your usage limit. Resets Mon 12:00am.",
    ];

    /// Text an agent prints that is not a stop: warnings, pauses, questions,
    /// code, prose about someone else's limit, and limits of other things.
    const NOT_STOPS: &[&str] = &[
        "You've used 85% of your weekly limit \u{b7} resets Oct 31, 12am (Asia/Makassar) \u{b7} try /effort medium",
        "Approaching weekly limit \u{b7} resets Mon 9am",
        "85% of your weekly limit used, resets Fri",
        "You've used 90% of your monthly limit. Your limit resets on Nov 1.",
        "Almost at your daily limit. Upgrade your plan for more.",
        "Quota exceeded for quota metric 'Requests' and limit 'Requests per minute'. Try again in a moment.",
        "Rate limit exceeded. Retrying in 20 seconds. You can upgrade for higher limits.",
        "Usage limit reached. Continuing with usage credits.",
        "5-hour limit resets 3pm - continuing with usage credits",
        "5-hour limit reached resets 3pm continuing with usage credits",
        "the retry loop stops when the limit reached flag is set",
        "print the usage limit to the console",
        "monthly limit",
        "ordinary work output with nothing about allowances",
        "what does hit your usage limit mean",
        "I hit the limit of the context window; resets after compaction.",
        "Check that the quota exceeded error is handled and try again after a delay.",
        "if the user has reached the limit, show the upgrade banner and wait",
        "fn on_quota_exceeded() { retry_after(30); } // try again later",
        "The build hit a memory limit and exited. Reset the cache and retry.",
        "You've reached your goal for today. The limit resets daily.",
        "The API rate limit is 60 requests; you have reached it. Try again later.",
        "Applied the diff. 3 files changed. Usage: cargo test",
        "Tests passed, no more failures remaining in the suite",
        "He ran out of patience with the usage docs",
        "Your balance sheet looks fine; the credit is applied until Friday.",
        "If you exceed the quota, wait until the reset",
        "Document how to handle quota exhausted errors: upgrade or wait.",
        "Add a banner that says you hit your weekly limit and resets Monday.",
        "When the quota is exhausted the service returns 429 and clients should retry after a delay.",
        "The user said they hit their limit yesterday and it resets tonight.",
        "The upload hit the size limit, so wait until the file is smaller.",
        "Tool output exceeded the limit of 30000 characters, truncated; try again with a narrower range",
        "Error: output limit reached. Try again with a narrower pattern.",
        // An allowance only partly used is never a stop, whatever the share.
        "You've used 50% of your weekly limit \u{b7} resets Mon 12:00am",
        "You've used 75% of your session limit \u{b7} resets 3pm",
        "You've used 90% of your Opus limit \u{b7} resets 3:45pm",
        "You've used 95% of your usage limit. Upgrade to continue.",
        "You've used 99% of your weekly limit \u{b7} resets Mon 12:00am",
        "Weekly limit 85% used \u{b7} resets Mon 9am",
        "You're at 90% of your weekly limit \u{b7} resets Mon 9am",
        "Only 10% of your usage limit remaining. Upgrade your plan.",
        "5% remaining until your weekly limit \u{b7} resets Mon 12:00am",
        "You have 10 messages left before your limit is reached. Upgrade your plan.",
        "You have 3 requests remaining before the quota is exhausted. Upgrade your plan.",
        "Close to your weekly limit \u{b7} resets Mon 9am",
        "You're approaching your usage limit. Upgrade to avoid interruptions.",
        // Lines of this repository, found by running the reader over it: source
        // that an agent prints is not an agent that has run out.
        "    /// Plan or account usage quota exhausted until a reset.",
        "        RunEnd::LimitReached(kind) => json!({\"end\": \"limit_reached\", \"limit\": kind.as_str()}),",
        "        \"limit_reached\" => StopReason::LimitReached,",
        "{\"event\": \"limit_reached\", \"kind\": \"usage_quota\", \"resets_at_ms\": null}",
        "/// Kind of limit a provider hit.\n#[non_exhaustive]\npub enum LimitKind {\n    /// Plan or account usage quota exhausted until a reset.\n    UsageQuota,\n    /// Short-term request rate limit.\n    RateLimit,\n    /// The conversation no longer fits the model's context window.\n    ContextWindow,\n}",
        "            .matches(\"working Daily limit reached. Try again tomorrow\")",
        "    assert!(stops(\"You've hit your weekly limit \\u{b7} resets Mon 12:00am\"));",
        // A list of messages in a source file or a translation file.
        "        \"You've hit your weekly limit \\u{b7} resets Mon 12:00am\",",
        "    \"Usage limit has been reached. Try again later.\",",
        "  \"limit.reached\": \"Quota exceeded. Upgrade at https://example.com/plans\"",
        // Words that look like a hint and are not.
        "Ran git reset --hard. The request limit reached flag was logged.",
        "Still waiting on CI while the job limit reached its default.",
    ];

    #[test]
    fn every_message_in_the_corpus_is_a_stop() {
        let missed: Vec<String> = STOPS
            .iter()
            .filter(|text| !stops(text))
            .map(|text| format!("score {}: {text}", best(text)))
            .collect();
        assert!(missed.is_empty(), "not found:\n{}", missed.join("\n"));
        assert!(STOPS.len() >= 25);
    }

    #[test]
    fn the_threshold_separates_the_two_corpora() {
        let lowest_stop = STOPS.iter().map(|t| best(t)).min().unwrap();
        let highest_other = NOT_STOPS.iter().map(|t| best(t)).max().unwrap();
        // Every text that is not a stop scores below every stop, and the
        // threshold lies between them.
        assert!(highest_other < lowest_stop, "{highest_other} {lowest_stop}");
        assert!(highest_other < THRESHOLD && THRESHOLD <= lowest_stop);
    }

    #[test]
    fn nothing_in_the_negative_corpus_is_a_stop() {
        let taken: Vec<String> = NOT_STOPS
            .iter()
            .filter(|text| stops(text))
            .map(|text| format!("score {}: {text}", best(text)))
            .collect();
        assert!(taken.is_empty(), "taken for a stop:\n{}", taken.join("\n"));
        assert!(NOT_STOPS.len() >= 25);
    }

    #[test]
    fn claude_weekly_and_session_limits_are_found_alike() {
        for kind in [
            "weekly", "session", "5-hour", "Opus", "Sonnet", "monthly", "daily",
        ] {
            let text = format!("You've hit your {kind} limit \u{b7} resets Mon 12:00am");
            assert!(stops(&text), "{text}");
        }
    }

    #[test]
    fn a_pause_for_a_limit_per_minute_is_not_a_stop() {
        assert!(!stops(
            "Quota exceeded for quota metric 'Requests' and limit 'Requests per minute'. Try again in a moment."
        ));
        assert!(!stops(
            "Rate limit exceeded. Retrying in 20 seconds. You can upgrade for higher limits."
        ));
        assert!(!stops(
            "Usage limit reached. Continuing with usage credits."
        ));
    }

    #[test]
    fn a_message_split_across_reads_is_found_once() {
        let found = signals(&[
            b"\x1b[31mYou've reached your mon",
            b"thly limit.\x1b[0m Your limit res",
            b"ets on Nov 1.\r\n",
            b"more text after it\r\n",
        ]);
        assert_eq!(found.len(), 1);
        assert!(
            found[0]
                .message
                .as_deref()
                .unwrap()
                .contains("monthly limit")
        );
        assert_eq!(found[0].kind, LimitKind::UsageQuota);
    }

    #[test]
    fn words_positioned_with_the_cursor_still_match() {
        assert!(stops(
            "Usage\x1b[1;20Hlimit\x1b[1;30Hreached.\x1b[2;1HAccess\x1b[2;10Hresets\x1b[2;20Hat 3 PM."
        ));
    }

    #[test]
    fn the_same_message_on_screen_does_not_fire_twice() {
        let found = signals(&[
            b"Daily limit reached. Try again tomorrow.\r\n",
            b"more output\r\n",
            b"still more output\r\n",
        ]);
        assert_eq!(found.len(), 1);
    }

    #[test]
    fn a_message_split_by_decoration_and_chunks_is_found_once() {
        let styled = "\x1b[2K\x1b[1;31m5-hour\x1b[0m\x1b[1Climit\x1b[1Creached\x1b[1C\u{2219}\x1b[1Cresets\x1b[1C3pm\r\n";
        assert_eq!(signals(&[styled.as_bytes()]).len(), 1);
        let found = signals(&[b"5-hour limit rea", b"ched \xe2\x88\x99 res", b"ets 3pm"]);
        assert_eq!(found.len(), 1);
        let long = signals(&[
            b"You've hit your usage limit. Upgrade to Pro (https://chatgpt.com/explore/pro), ",
            b"visit https://chatgpt.com/settings/usage to purchase more cre",
            b"dits or try again at 7:11 PM.",
        ]);
        assert_eq!(long.len(), 1);
    }

    #[test]
    fn a_usage_warning_below_the_limit_is_not_a_stop_but_the_same_words_after_it_are() {
        assert!(!stops(
            "You've used 85% of your weekly limit \u{b7} resets Oct 31, 12am (Asia/Makassar) \u{b7} try /effort medium"
        ));
        assert!(stops(
            "Weekly limit reached \u{b7} resets Oct 31, 12am (Asia/Makassar)"
        ));
    }

    #[test]
    fn a_hint_far_from_the_phrase_is_not_the_same_message() {
        let far = format!(
            "You've hit your usage limit. {} try again",
            "x ".repeat(300)
        );
        assert!(!stops(&far));
    }

    #[test]
    fn text_that_looked_like_a_stop_and_was_turned_down_is_reported() {
        let mut decoder = UniversalLimitDecoder::new();
        let mut out = Vec::new();
        decoder.decode(
            TransportFrame::Output(b"You've hit your weekly limit"),
            &mut out,
        );
        let diagnostics: Vec<String> = out
            .iter()
            .filter_map(|d| match &d.event {
                AgentEvent::Diagnostic { message } => Some(message.clone()),
                _ => None,
            })
            .collect();
        assert_eq!(diagnostics.len(), 1, "{out:?}");
        assert!(diagnostics[0].contains("weekly limit"), "{diagnostics:?}");
        // The rest of the message comes, and it is a stop.
        decoder.decode(
            TransportFrame::Output(b" \xc2\xb7 resets Mon 12:00am"),
            &mut out,
        );
        assert!(
            out.iter()
                .any(|d| matches!(d.event, AgentEvent::LimitReached(_)))
        );
    }

    #[test]
    fn a_message_among_other_screen_text_is_found_like_one_on_its_own() {
        // A terminal screen is text with no sentences: what stands before the
        // message is whatever was drawn earlier, and it may speak to the
        // reader too.
        let noise = "working on the parser with you since the morning and the tests are green \
                     for the module that reads the configuration file from disk ";
        let on_screen = |message: &&str| format!("{noise}\r\n{message}\r\n");
        let missed: Vec<&&str> = STOPS.iter().filter(|m| !stops(&on_screen(m))).collect();
        assert!(
            missed.is_empty(),
            "not found among other text:\n{missed:#?}"
        );
        let taken: Vec<&&str> = NOT_STOPS.iter().filter(|m| stops(&on_screen(m))).collect();
        assert!(
            taken.is_empty(),
            "taken for a stop among other text:\n{taken:#?}"
        );
    }

    /// A long text goes through the rolling window a line at a time, and the
    /// window is cut as it goes. A cut in the middle of a line must not take
    /// away the part of it that says it is source (or a note), or what is left
    /// would read as a message.
    #[test]
    fn a_long_text_cut_by_the_window_never_turns_into_a_stop() {
        let source = NOT_STOPS
            .iter()
            .filter(|t| t.contains("/// Plan or account"))
            .copied()
            .collect::<Vec<_>>()
            .join("\n");
        assert!(!source.is_empty());
        let mut decoder = UniversalLimitDecoder::new();
        let mut out = Vec::new();
        for round in 0..40 {
            for line in source.split('\n') {
                // The lines are different in each round, so the cuts fall
                // on different parts of them.
                let line = format!("{}{line}\r\n", " ".repeat(round % 7));
                decoder.decode(TransportFrame::Output(line.as_bytes()), &mut out);
            }
        }
        let stops = out
            .iter()
            .filter(|d| matches!(d.event, AgentEvent::LimitReached(_)))
            .count();
        assert_eq!(stops, 0, "{out:#?}");
    }

    #[test]
    fn a_stretch_that_was_turned_down_does_not_hide_a_stop_after_it() {
        // The first line scores high and is turned down for how it is worded;
        // the stop on the next line scores lower and is still a stop.
        let text = "He hit the limit; wait until tomorrow, upgrade, resets Monday 9am\r\nQuota exceeded. Upgrade at https://example.com/plans\r\n";
        assert!(stops(text));
    }

    #[test]
    fn the_warning_earlier_on_the_screen_does_not_hide_the_limit_that_follows() {
        // The warning came first, and stays on the screen above the stop.
        let warning = "You've used 85% of your weekly limit \u{b7} resets Mon 12:00am\r\n";
        let stop = "You've hit your weekly limit \u{b7} resets Mon 12:00am\r\n";
        assert!(!stops(warning));
        assert!(stops(&format!("{warning}{stop}")));
        // As the decoder sees it, a read at a time.
        let found = signals(&[warning.as_bytes(), b"more work\r\n", stop.as_bytes()]);
        assert_eq!(found.len(), 1);
    }

    #[test]
    fn only_an_allowance_that_is_partly_used_is_set_aside() {
        let partly = |text: &str| {
            let normalized = normalize(text.as_bytes());
            partly_used(&normalized, &words(&normalized))
        };
        for text in [
            "85% used",
            "99% of your limit",
            "50% remaining",
            "only 10% left",
            "10 messages left",
            "3 requests remaining",
        ] {
            assert!(partly(text), "{text}");
        }
        // None left is all gone, and all used is all used.
        for text in [
            "100% of your limit",
            "100% used",
            "0% remaining",
            "0 messages left",
            "0 requests remaining",
            "no credits remaining",
            "limit reached",
        ] {
            assert!(!partly(text), "{text}");
        }
    }

    #[test]
    fn the_lines_that_say_the_allowance_is_gone_are_taken_out_of_a_text() {
        let text = "User: fix the parser\nAgent: done\n\u{203a} You've hit your usage limit. Upgrade to Pro (https://chatgpt.com/explore/pro) or try again at 7:11 PM.\nUser: thanks\n";
        let redacted = redact_limit_text(text);
        assert!(redacted.contains("fix the parser"), "{redacted}");
        assert!(redacted.contains("thanks"), "{redacted}");
        assert!(redacted.contains("[usage limit message removed]"));
        assert!(!redacted.to_lowercase().contains("hit your usage limit"));
        assert!(
            candidates(&normalize(redacted.as_bytes()))
                .iter()
                .all(|c| !c.is_stop())
        );
        // A message over two lines goes too, and ordinary text is left alone.
        let two = "ok\nYou've hit your weekly limit\n\u{b7} resets Mon 12:00am\nnext";
        let redacted = redact_limit_text(two);
        assert!(!redacted.contains("weekly"), "{redacted}");
        assert!(redacted.starts_with("ok\n") && redacted.ends_with("next"));
        assert_eq!(
            redact_limit_text("nothing here\nat all"),
            "nothing here\nat all"
        );
    }
}
