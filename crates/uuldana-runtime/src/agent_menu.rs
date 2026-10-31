//! The `/agent` menu: the user picks which agent works first, which one takes
//! over next, and so on.
//!
//! This module is only the menu itself: keys in, a screen out. It touches no
//! terminal and no provider, so the orchestrator decides when to show it and
//! what to do with the answer.

use crate::config::ProviderEntry;

/// One agent the user can put into the order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Candidate {
    /// The configured entry, or a detected agent that is not configured yet.
    pub entry: ProviderEntry,
    /// This entry is running now.
    pub active: bool,
    /// Whole minutes this entry still rests after a usage limit.
    pub resting_minutes: Option<u64>,
    /// The entry is not in `config.json` yet; choosing it adds it.
    pub new: bool,
}

/// A key the menu understands.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Key {
    /// Move the highlight up.
    Up,
    /// Move the highlight down.
    Down,
    /// Take the highlighted row.
    Enter,
    /// Leave without changing anything.
    Cancel,
    /// Take the row with this number (1 to 9).
    Number(usize),
}

/// Turns the bytes of one read from the terminal into keys. Anything the menu
/// does not use is dropped, so a stray key never changes the order.
pub fn parse_keys(bytes: &[u8]) -> Vec<Key> {
    let mut keys = Vec::new();
    let mut at = 0;
    while at < bytes.len() {
        let rest = &bytes[at..];
        let (key, used) = match rest {
            [0x1b, b'[', b'A', ..] | [0x1b, b'O', b'A', ..] => (Some(Key::Up), 3),
            [0x1b, b'[', b'B', ..] | [0x1b, b'O', b'B', ..] => (Some(Key::Down), 3),
            // A lone ESC is the Escape key; ESC followed by more is a longer
            // sequence this menu has no use for.
            [0x1b] => (Some(Key::Cancel), 1),
            [0x1b, b'[', ..] | [0x1b, b'O', ..] => (None, escape_length(rest)),
            [0x1b, ..] => (None, 2),
            [0x03, ..] => (Some(Key::Cancel), 1),
            [b'\r', b'\n', ..] => (Some(Key::Enter), 2),
            [b'\r', ..] | [b'\n', ..] => (Some(Key::Enter), 1),
            [b'k', ..] => (Some(Key::Up), 1),
            [b'j', ..] => (Some(Key::Down), 1),
            [digit @ b'1'..=b'9', ..] => (Some(Key::Number(usize::from(digit - b'0'))), 1),
            _ => (None, 1),
        };
        if let Some(key) = key {
            keys.push(key);
        }
        at += used;
    }
    keys
}

/// Length of a CSI or SS3 sequence at the start of `bytes`.
fn escape_length(bytes: &[u8]) -> usize {
    let mut at = 2;
    while at < bytes.len() {
        let byte = bytes[at];
        at += 1;
        if (0x40..=0x7e).contains(&byte) {
            break;
        }
    }
    at
}

/// Where the menu stands after a key.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Step {
    /// Waiting for more keys.
    Pending,
    /// The user left without choosing.
    Cancelled,
    /// The user finished. Indexes into the candidates, first agent first.
    Chosen(Vec<usize>),
}

/// The menu state.
#[derive(Debug, Clone)]
pub struct AgentMenu {
    candidates: Vec<Candidate>,
    chosen: Vec<usize>,
    highlight: usize,
}

/// The extra row that ends the picking after at least one agent.
const DONE: &str = "No more, keep the rest in their current order";

impl AgentMenu {
    /// Starts a menu over `candidates`, with the running agent highlighted.
    /// There must be at least one candidate.
    pub fn new(candidates: Vec<Candidate>) -> Self {
        assert!(!candidates.is_empty(), "the agent menu needs a candidate");
        let highlight = candidates.iter().position(|c| c.active).unwrap_or(0);
        Self {
            candidates,
            chosen: Vec::new(),
            highlight,
        }
    }

    /// Candidates not picked yet, in their original order.
    fn open(&self) -> Vec<usize> {
        (0..self.candidates.len())
            .filter(|index| !self.chosen.contains(index))
            .collect()
    }

    /// Rows of the current question: the open candidates, then "no more" once
    /// something is chosen and more could still be added.
    fn rows(&self) -> usize {
        self.open().len() + usize::from(self.offers_done())
    }

    fn offers_done(&self) -> bool {
        !self.chosen.is_empty() && !self.open().is_empty()
    }

    /// Applies one key.
    pub fn press(&mut self, key: Key) -> Step {
        match key {
            Key::Cancel => return Step::Cancelled,
            Key::Up => self.highlight = self.highlight.saturating_sub(1),
            Key::Down => self.highlight = (self.highlight + 1).min(self.rows() - 1),
            Key::Number(number) => {
                if number > self.rows() {
                    return Step::Pending;
                }
                self.highlight = number - 1;
                return self.press(Key::Enter);
            }
            Key::Enter => {
                let open = self.open();
                if let Some(&index) = open.get(self.highlight) {
                    self.chosen.push(index);
                } else {
                    // The "no more" row.
                    return Step::Chosen(self.chosen.clone());
                }
                if self.open().is_empty() {
                    return Step::Chosen(self.chosen.clone());
                }
                self.highlight = 0;
            }
        }
        Step::Pending
    }

    /// The question being asked: "first", "second", and so on.
    fn ordinal(&self) -> &'static str {
        match self.chosen.len() {
            0 => "first",
            1 => "second",
            2 => "third",
            3 => "fourth",
            4 => "fifth",
            _ => "next",
        }
    }

    /// Draws the menu. Rows use no more than `width` columns.
    pub fn render(&self, width: usize) -> String {
        let mut lines = vec![
            format!("Agent order ({} agent to use)", self.ordinal()),
            String::new(),
        ];
        if self.chosen.is_empty() {
            lines.push("Pick the agent that works first. When it reaches its usage".into());
            lines.push("limit, the next one you pick here takes over.".into());
        } else {
            let picked: Vec<_> = self
                .chosen
                .iter()
                .map(|&index| self.candidates[index].entry.label.as_str())
                .collect();
            lines.push(format!("So far: {}", picked.join(" > ")));
        }
        lines.push(String::new());
        let open = self.open();
        for (row, &index) in open.iter().enumerate() {
            lines.push(self.row(row, &self.candidates[index]));
        }
        if self.offers_done() {
            lines.push(format!(
                "{} {}. {DONE}",
                if self.highlight == open.len() {
                    ">"
                } else {
                    " "
                },
                open.len() + 1
            ));
        }
        lines.push(String::new());
        lines.push("Up/Down or a number to choose, Enter to confirm, Esc to leave".into());
        lines
            .into_iter()
            .map(|line| clip(&line, width))
            .collect::<Vec<_>>()
            .join("\r\n")
    }

    fn row(&self, row: usize, candidate: &Candidate) -> String {
        let mut notes = Vec::new();
        if candidate.active {
            notes.push("running now".to_owned());
        }
        if let Some(minutes) = candidate.resting_minutes {
            notes.push(format!(
                "resting after a usage limit, about {minutes} min left; choosing it ends the rest"
            ));
        }
        if candidate.new {
            notes.push("not in config.json yet, choosing it adds it".to_owned());
        }
        let note = if notes.is_empty() {
            String::new()
        } else {
            format!("  ({})", notes.join("; "))
        };
        format!(
            "{} {}. {}{note}",
            if row == self.highlight { ">" } else { " " },
            row + 1,
            candidate.entry.label
        )
    }
}

/// Cuts `line` to `width` characters, ending with `...` when it was cut.
fn clip(line: &str, width: usize) -> String {
    if width < 8 || line.chars().count() <= width {
        return line.to_owned();
    }
    let kept: String = line.chars().take(width - 3).collect();
    format!("{kept}...")
}

#[cfg(test)]
mod tests {
    use super::*;
    use uuldana_core::ProviderId;

    fn candidate(label: &str, active: bool) -> Candidate {
        Candidate {
            entry: ProviderEntry {
                id: ProviderId::new(label).unwrap(),
                label: label.to_owned(),
                env: Vec::new(),
                cooldown: std::time::Duration::from_secs(60),
                auto: false,
            },
            active,
            resting_minutes: None,
            new: false,
        }
    }

    fn menu() -> AgentMenu {
        AgentMenu::new(vec![
            candidate("agent-a", true),
            candidate("agent-b", false),
            candidate("agent-c", false),
        ])
    }

    #[test]
    fn keys_cover_arrows_enter_numbers_and_escape() {
        assert_eq!(parse_keys(b"\x1b[A\x1b[B"), [Key::Up, Key::Down]);
        assert_eq!(parse_keys(b"\x1bOB"), [Key::Down]);
        assert_eq!(parse_keys(b"\r"), [Key::Enter]);
        assert_eq!(parse_keys(b"\r\n"), [Key::Enter]);
        assert_eq!(parse_keys(b"2"), [Key::Number(2)]);
        assert_eq!(parse_keys(b"\x1b"), [Key::Cancel]);
        assert_eq!(parse_keys(b"\x03"), [Key::Cancel]);
    }

    #[test]
    fn unused_keys_and_longer_sequences_change_nothing() {
        assert!(parse_keys(b"x0").is_empty());
        assert!(parse_keys(b"\x1b[3~").is_empty());
        assert!(parse_keys(b"\x1b[1;5C").is_empty());
        // A sequence the menu skips does not swallow the key after it.
        assert_eq!(parse_keys(b"\x1b[3~\r"), [Key::Enter]);
    }

    #[test]
    fn picking_in_order_builds_the_order() {
        let mut menu = menu();
        assert_eq!(menu.press(Key::Number(2)), Step::Pending);
        // Second question: agent-a and agent-c remain.
        assert_eq!(menu.press(Key::Number(2)), Step::Pending);
        // Only agent-a remains, so the last pick ends the menu.
        assert_eq!(menu.press(Key::Enter), Step::Chosen(vec![1, 2, 0]));
    }

    #[test]
    fn done_row_keeps_the_rest_in_place() {
        let mut menu = menu();
        assert_eq!(menu.press(Key::Down), Step::Pending);
        assert_eq!(menu.press(Key::Enter), Step::Pending);
        // Rows: agent-a, agent-c, then the "no more" row.
        assert_eq!(menu.press(Key::Number(3)), Step::Chosen(vec![1]));
    }

    #[test]
    fn there_is_no_done_row_before_the_first_pick() {
        let mut menu = menu();
        assert_eq!(menu.press(Key::Number(4)), Step::Pending);
        assert!(!menu.render(80).contains(DONE));
    }

    #[test]
    fn escape_cancels_at_any_step() {
        let mut menu = menu();
        menu.press(Key::Number(1));
        assert_eq!(menu.press(Key::Cancel), Step::Cancelled);
    }

    #[test]
    fn highlight_starts_on_the_running_agent_and_stays_in_range() {
        let mut menu = AgentMenu::new(vec![
            candidate("agent-b", false),
            candidate("agent-c", true),
        ]);
        assert!(menu.render(80).contains("> 2. agent-c"));
        for _ in 0..5 {
            menu.press(Key::Down);
        }
        assert!(menu.render(80).contains("> 2. agent-c"));
        for _ in 0..5 {
            menu.press(Key::Up);
        }
        assert!(menu.render(80).contains("> 1. agent-b"));
    }

    #[test]
    fn render_names_the_step_notes_and_fits_the_width() {
        let mut rows = vec![candidate("agent-a", true), candidate("agent-b", false)];
        rows[1].resting_minutes = Some(42);
        rows[1].new = true;
        let mut menu = AgentMenu::new(rows);
        let first = menu.render(120);
        assert!(first.contains("first agent"));
        assert!(first.contains("agent-a  (running now)"));
        assert!(first.contains("about 42 min left"));
        assert!(first.contains("not in config.json yet"));
        menu.press(Key::Number(1));
        let second = menu.render(120);
        assert!(second.contains("second agent"));
        assert!(second.contains("So far: agent-a"));
        for line in menu.render(30).split("\r\n") {
            assert!(line.chars().count() <= 30, "{line}");
        }
    }
}
