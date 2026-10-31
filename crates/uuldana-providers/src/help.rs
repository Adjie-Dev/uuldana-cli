//! Reading the `--help` text of a program.
//!
//! Command line tools describe themselves in a few common layouts: a list of
//! options with their values, a list of subcommands, and a usage line with
//! positional arguments. This module turns that text into plain data and knows
//! nothing about any particular program.

/// Whether an option takes a value, and what the help calls it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Value {
    /// A switch.
    None,
    /// `<id>`: the option needs a value.
    Required(String),
    /// `[id]`: the value may be left out.
    Optional(String),
}

/// One option as the help lists it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct HelpOption {
    /// Every spelling, for example `-r` and `--resume`.
    pub names: Vec<String>,
    pub value: Value,
    /// Description, lines joined with spaces.
    pub text: String,
    /// Values the help says the option accepts.
    pub choices: Vec<String>,
}

impl HelpOption {
    /// The long spellings without the dashes, lowercase.
    pub fn longs(&self) -> impl Iterator<Item = String> + '_ {
        self.names
            .iter()
            .filter_map(|name| name.strip_prefix("--"))
            .map(str::to_lowercase)
    }

    /// The first long spelling with its dashes, or the first spelling.
    pub fn spelling(&self) -> &str {
        self.names
            .iter()
            .find(|name| name.starts_with("--"))
            .or_else(|| self.names.first())
            .map_or("", String::as_str)
    }

    /// Takes a value that must be given.
    pub fn needs_value(&self) -> bool {
        matches!(self.value, Value::Required(_))
    }

    /// Takes a value, given or not.
    pub fn takes_value(&self) -> bool {
        !matches!(self.value, Value::None)
    }
}

/// A subcommand.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct HelpCommand {
    pub name: String,
    pub text: String,
}

/// What a `--help` text said.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct Help {
    pub usage: String,
    pub options: Vec<HelpOption>,
    pub commands: Vec<HelpCommand>,
    /// Positional arguments by name, lowercase, from the arguments section and
    /// the usage line.
    pub positionals: Vec<String>,
    /// The whole text, lowercase.
    pub lower: String,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Section {
    Options,
    Commands,
    Arguments,
    Other,
}

fn indent(line: &str) -> usize {
    line.len() - line.trim_start().len()
}

fn strip_escapes(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\u{1b}' {
            if chars.peek() == Some(&'[') {
                chars.next();
                for next in chars.by_ref() {
                    if ('\u{40}'..='\u{7e}').contains(&next) {
                        break;
                    }
                }
            }
        } else if c != '\r' {
            out.push(c);
        }
    }
    out
}

/// A heading is a line at the left edge that names a section.
fn heading(line: &str) -> Option<Section> {
    if line.is_empty() || indent(line) > 0 {
        return None;
    }
    let lower = line.trim().trim_end_matches(':').to_lowercase();
    if !line.trim_end().ends_with(':') && lower.split_whitespace().count() > 2 {
        return None;
    }
    if lower.starts_with("usage") {
        return Some(Section::Other);
    }
    if lower.contains("option") || lower.contains("flag") {
        Some(Section::Options)
    } else if lower.contains("command") {
        Some(Section::Commands)
    } else if lower.contains("argument") || lower.contains("positional") {
        Some(Section::Arguments)
    } else if line.trim_end().ends_with(':') {
        Some(Section::Other)
    } else {
        None
    }
}

/// A line that begins another option next to the one at indentation `at`.
/// Short flags shift the column of the long ones by a few characters; a
/// wrapped description that happens to start with a flag is indented far
/// deeper than that. A bullet (`- name: ...`) is a description, not an option.
fn starts_a_sibling(line: &str, at: usize) -> bool {
    let trimmed = line.trim_start();
    let mut chars = trimmed.chars();
    let (first, second) = (chars.next(), chars.next());
    first == Some('-')
        && second.is_some_and(|c| c.is_alphanumeric() || c == '-')
        && indent(line) <= at + 6
}

/// Splits `spec  description` at the first run of two or more spaces.
fn split_columns(text: &str) -> (&str, &str) {
    match text.find("  ") {
        Some(at) => (&text[..at], text[at..].trim()),
        None => (text, ""),
    }
}

fn parse_spelling(token: &str) -> Option<(String, Value)> {
    let token = token.trim();
    if !token.starts_with('-') {
        return None;
    }
    let (name, rest) = match token.find([' ', '=']) {
        Some(at) => (&token[..at], token[at + 1..].trim()),
        None => (token, ""),
    };
    if name.len() < 2
        || !name
            .chars()
            .skip_while(|c| *c == '-')
            .all(|c| c.is_alphanumeric() || c == '-' || c == '_' || c == '.')
    {
        return None;
    }
    let value = if rest.is_empty() {
        Value::None
    } else if rest.starts_with('[') {
        Value::Optional(rest.to_owned())
    } else {
        Value::Required(rest.to_owned())
    };
    Some((name.to_owned(), value))
}

fn parse_option(spec: &str, text: String) -> Option<HelpOption> {
    let mut names = Vec::new();
    let mut value = Value::None;
    for token in spec.split(", ") {
        let (name, found) = parse_spelling(token)?;
        names.push(name);
        if found != Value::None {
            value = found;
        }
    }
    let choices = choices_in(&text);
    Some(HelpOption {
        names,
        value,
        text,
        choices,
    })
}

fn clean_choice(item: &str) -> String {
    item.trim()
        .trim_matches(|c: char| {
            c == '"' || c == '\'' || c == '`' || c == '.' || c == ')' || c == ']'
        })
        .trim()
        .to_owned()
}

/// Values listed after `choices:` or `possible values:`, or as bullets.
fn choices_in(text: &str) -> Vec<String> {
    let lower = text.to_lowercase();
    for marker in ["choices:", "possible values:"] {
        if let Some(at) = lower.find(marker) {
            let tail = &text[at + marker.len()..];
            let end = tail.find([')', ']']).unwrap_or(tail.len());
            let listed: Vec<String> = tail[..end]
                .split(',')
                .map(clean_choice)
                .filter(|item| !item.is_empty() && !item.contains(' '))
                .collect();
            if !listed.is_empty() {
                return listed;
            }
            // Bullets: "- name: what it does".
            let bullets: Vec<String> = tail
                .split(" - ")
                .skip(1)
                .filter_map(|item| item.split(':').next())
                .map(clean_choice)
                .filter(|item| !item.is_empty() && !item.contains(' '))
                .collect();
            if !bullets.is_empty() {
                return bullets;
            }
        }
    }
    Vec::new()
}

/// Reads a help text.
pub(crate) fn parse(text: &str) -> Help {
    let text = strip_escapes(text);
    let lines: Vec<&str> = text.lines().collect();
    let mut help = Help {
        lower: text.to_lowercase(),
        ..Help::default()
    };
    let mut section = Section::Other;
    let mut index = 0;
    while index < lines.len() {
        let line = lines[index];
        if let Some(found) = heading(line) {
            section = found;
            if line.trim_start().to_lowercase().starts_with("usage") {
                help.usage.push_str(line.trim());
                index += 1;
                while index < lines.len() && indent(lines[index]) > 0 {
                    help.usage.push(' ');
                    help.usage.push_str(lines[index].trim());
                    index += 1;
                }
                continue;
            }
            index += 1;
            continue;
        }
        let at = indent(line);
        let trimmed = line.trim();
        if trimmed.is_empty() || at == 0 {
            index += 1;
            continue;
        }
        // Continuation lines belong to the entry that started them.
        let mut body = Vec::new();
        let mut next = index + 1;
        while next < lines.len() {
            let more = lines[next];
            if more.trim().is_empty() {
                // A blank line inside an entry (long layouts) is kept only if
                // the line after it is still deeper than the entry.
                match lines.get(next + 1) {
                    Some(after)
                        if indent(after) > at
                            && !after.trim().is_empty()
                            && !starts_a_sibling(after, at) =>
                    {
                        next += 1;
                        continue;
                    }
                    _ => break,
                }
            }
            if indent(more) > at && !starts_a_sibling(more, at) {
                body.push(more.trim());
                next += 1;
            } else {
                break;
            }
        }
        match section {
            Section::Commands => {
                let (spec, desc) = split_columns(trimmed);
                let name = spec.split_whitespace().next().unwrap_or("").to_owned();
                if !name.is_empty() && !name.starts_with('-') {
                    let mut text = desc.to_owned();
                    for part in &body {
                        text.push(' ');
                        text.push_str(part);
                    }
                    help.commands.push(HelpCommand {
                        name,
                        text: text.trim().to_owned(),
                    });
                }
            }
            Section::Arguments => {
                let (spec, _) = split_columns(trimmed);
                let name = spec
                    .trim_matches(|c| c == '[' || c == ']' || c == '<' || c == '>')
                    .to_lowercase();
                if !name.is_empty() && !name.starts_with('-') {
                    help.positionals.push(name);
                }
            }
            Section::Options | Section::Other => {
                if trimmed.starts_with('-') {
                    let (spec, desc) = split_columns(trimmed);
                    let mut text = desc.to_owned();
                    for part in &body {
                        text.push(' ');
                        text.push_str(part);
                    }
                    if let Some(option) = parse_option(spec, text.trim().to_owned()) {
                        help.options.push(option);
                    }
                }
            }
        }
        index = next;
    }
    // Positionals named on the usage line, for layouts without an arguments
    // section.
    for word in help.usage.split_whitespace() {
        let inner = word.trim_matches(|c| c == '[' || c == ']' || c == '<' || c == '>' || c == '.');
        let wrapped = word.starts_with('[') || word.starts_with('<');
        if wrapped
            && !inner.is_empty()
            && !inner.starts_with('-')
            && !matches!(
                inner.to_lowercase().as_str(),
                "options" | "command" | "args" | "subcommand"
            )
        {
            let name = inner.to_lowercase();
            if !help.positionals.contains(&name) {
                help.positionals.push(name);
            }
        }
    }
    help
}

#[cfg(test)]
mod tests {
    use super::*;

    const A: &str = include_str!("../tests/fixtures/help/a-top.txt");
    const B_TOP: &str = include_str!("../tests/fixtures/help/b-top.txt");
    const B_RESUME: &str = include_str!("../tests/fixtures/help/b-resume.txt");
    const C_TOP: &str = include_str!("../tests/fixtures/help/c-top.txt");
    const C_CHAT: &str = include_str!("../tests/fixtures/help/c-chat.txt");
    const D: &str = include_str!("../tests/fixtures/help/d-top.txt");

    fn option<'a>(help: &'a Help, long: &str) -> &'a HelpOption {
        help.options
            .iter()
            .find(|option| option.longs().any(|name| name == long))
            .unwrap_or_else(|| panic!("no option --{long}"))
    }

    #[test]
    fn options_with_and_without_values_are_told_apart() {
        let help = parse(A);
        assert_eq!(option(&help, "continue").value, Value::None);
        assert!(option(&help, "continue").names.contains(&"-c".to_owned()));
        assert!(matches!(option(&help, "resume").value, Value::Optional(_)));
        assert!(matches!(
            option(&help, "session-id").value,
            Value::Required(_)
        ));
        let help = parse(C_CHAT);
        assert_eq!(option(&help, "resume").value, Value::None);
        assert!(option(&help, "resume-id").needs_value());
        assert_eq!(option(&help, "trust-all-tools").value, Value::None);
    }

    #[test]
    fn choices_come_from_parentheses_brackets_and_bullets() {
        let help = parse(A);
        let mode = option(&help, "permission-mode");
        assert!(mode.choices.contains(&"acceptEdits".to_owned()), "{mode:?}");
        assert!(mode.choices.contains(&"plan".to_owned()));
        assert!(mode.choices.contains(&"manual".to_owned()));
        let help = parse(D);
        let mode = option(&help, "permission-mode");
        assert!(
            mode.choices.contains(&"accept_edits".to_owned()),
            "{mode:?}"
        );
        let help = parse(B_RESUME);
        let sandbox = option(&help, "sandbox");
        assert!(
            sandbox.choices.contains(&"read-only".to_owned()),
            "{sandbox:?}"
        );
        assert!(sandbox.choices.contains(&"workspace-write".to_owned()));
        let approval = option(&help, "ask-for-approval");
        assert!(
            approval.choices.contains(&"never".to_owned()),
            "{approval:?}"
        );
    }

    #[test]
    fn subcommands_and_positionals_are_listed() {
        let top = parse(B_TOP);
        assert!(top.commands.iter().any(|c| c.name == "resume"));
        assert!(
            top.positionals.contains(&"prompt".to_owned()),
            "{:?}",
            top.positionals
        );
        let resume = parse(B_RESUME);
        assert!(
            resume.positionals.contains(&"session_id".to_owned()),
            "{:?}",
            resume.positionals
        );
        assert!(
            option(&resume, "last")
                .text
                .to_lowercase()
                .contains("most recent")
        );
        let two_levels = parse(C_TOP);
        assert!(two_levels.commands.iter().any(|c| c.name == "chat"));
        let chat = parse(C_CHAT);
        assert!(
            chat.positionals.contains(&"input".to_owned()),
            "{:?}",
            chat.positionals
        );
        assert!(parse(A).positionals.contains(&"prompt".to_owned()));
    }

    #[test]
    fn a_prompt_interactive_option_is_found() {
        let help = parse(D);
        let interactive = option(&help, "prompt-interactive");
        assert!(interactive.needs_value());
        assert!(interactive.names.contains(&"-i".to_owned()));
    }

    #[test]
    fn text_that_is_not_a_help_text_gives_nothing() {
        let help = parse("error: unknown flag\n");
        assert!(help.options.is_empty() && help.commands.is_empty());
        let help = parse(
            "\u{1b}[1mUsage:\u{1b}[0m tool [OPTIONS]\n\nOptions:\n  -h, --help  Print help\n",
        );
        assert_eq!(help.options.len(), 1);
        assert_eq!(help.options[0].names, ["-h", "--help"]);
    }
}
