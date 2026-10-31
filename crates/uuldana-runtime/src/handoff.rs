//! Turning a [`HandoffPackage`] into the first prompt of the next provider.
//!
//! Only visible context moves: the objective and the visible conversation. The
//! text says so, and lists what does not move, so the receiving agent does not
//! assume it inherited hidden reasoning, memory or approvals. It must verify
//! the state of the workspace instead of trusting a summary.

use uuldana_core::{
    AgentEvent, EventEnvelope, HandoffPackage, NonTransferable, Role, RunId, TranscriptEntry,
};

/// Upper bound for the rendered prompt. It travels as one command line
/// argument, and Windows limits a command line to 32,767 characters.
pub const MAX_PROMPT_CHARS: usize = 20_000;

/// Conversation text taken from a run's normalized events, for providers whose
/// own store cannot be read. Consecutive chunks by the same author are joined.
pub fn transcript_from_events(events: &[EventEnvelope], run: &RunId) -> Vec<TranscriptEntry> {
    let mut out: Vec<TranscriptEntry> = Vec::new();
    for envelope in events.iter().filter(|e| &e.run == run) {
        let AgentEvent::MessageChunk { role, text } = &envelope.event else {
            continue;
        };
        match out.last_mut() {
            Some(last) if last.role == *role && last.fidelity == envelope.fidelity => {
                last.text.push_str(text);
            }
            _ => out.push(TranscriptEntry {
                role: *role,
                text: text.clone(),
                fidelity: envelope.fidelity,
            }),
        }
    }
    for entry in &mut out {
        entry.text = entry.text.trim().to_owned();
    }
    out.retain(|entry| !entry.text.is_empty());
    out
}

/// The screen text recorded for `run`, the latest if there are several.
pub fn screen_from_events(events: &[EventEnvelope], run: &RunId) -> Option<String> {
    events
        .iter()
        .rev()
        .filter(|envelope| &envelope.run == run)
        .find_map(|envelope| match &envelope.event {
            AgentEvent::Extension {
                namespace,
                name,
                payload,
            } if namespace == crate::supervisor::SCREEN_NAMESPACE
                && name == crate::supervisor::SCREEN_EVENT =>
            {
                Some(payload.clone())
            }
            _ => None,
        })
        .filter(|text| !text.trim().is_empty())
}

fn describe(item: NonTransferable) -> &'static str {
    match item {
        NonTransferable::HiddenReasoning => "reasoning that was not displayed",
        NonTransferable::NativeMemory => "the previous agent's memory and learned preferences",
        NonTransferable::ProviderApprovals => {
            "approvals granted in the previous agent (you must ask again where needed)"
        }
        NonTransferable::ProviderInternalState => "any other internal state",
        NonTransferable::InFlightToolCalls => "tool calls that were still running",
        _ => "other internal state",
    }
}

/// Renders the first prompt for the next provider. `from` names the previous
/// provider entry and `reason` why the work moved, for example a usage limit.
pub fn render(package: &HandoffPackage, from: &str, reason: &str) -> String {
    render_with_screen(package, from, reason, None)
}

/// [`render`] with the text the previous agent's terminal showed. It is used
/// only when there is no conversation to carry, which is the case for an agent
/// whose own store cannot be read: the screen is then the only record of what
/// was asked and answered, interface text included.
pub fn render_with_screen(
    package: &HandoffPackage,
    from: &str,
    reason: &str,
    screen: Option<&str>,
) -> String {
    let mut head = format!(
        "You are continuing work that another coding agent ({from}) started in this workspace. \
         It stopped because {reason}. Continue from where it left off. Do not redo finished \
         work, and check the current files and git status before relying on the summary below.\n"
    );
    if let Some(objective) = &package.objective {
        head.push_str(&format!("\nOriginal request:\n{objective}\n"));
    }
    let mut tail = String::from("\nNot carried over: ");
    tail.push_str(
        &package
            .not_transferred()
            .iter()
            .map(|item| describe(*item))
            .collect::<Vec<_>>()
            .join("; "),
    );
    tail.push_str(".\n");

    let lines: Vec<String> = package
        .transcript
        .iter()
        .map(|entry| {
            let who = match entry.role {
                Role::User => "User",
                _ => "Agent",
            };
            format!("{who}: {}\n", entry.text.trim())
        })
        .collect();
    let budget = MAX_PROMPT_CHARS.saturating_sub(head.chars().count() + tail.chars().count() + 120);
    let mut kept: Vec<&String> = Vec::new();
    let mut used = 0;
    for line in lines.iter().rev() {
        let size = line.chars().count();
        if used + size > budget {
            break;
        }
        used += size;
        kept.push(line);
    }
    let omitted = lines.len() - kept.len();
    let mut body = String::new();
    if !kept.is_empty() || omitted > 0 {
        body.push_str("\nVisible conversation so far");
        if omitted > 0 {
            body.push_str(&format!(" ({omitted} earlier messages omitted)"));
        }
        body.push_str(":\n");
        for line in kept.iter().rev() {
            body.push_str(line);
        }
    }
    if body.is_empty()
        && let Some(screen) = screen.map(str::trim).filter(|text| !text.is_empty())
    {
        let budget =
            MAX_PROMPT_CHARS.saturating_sub(head.chars().count() + tail.chars().count() + 400);
        let total = screen.chars().count();
        let kept: String = screen.chars().skip(total.saturating_sub(budget)).collect();
        body.push_str(
            "\nThe previous agent's own history could not be read. This is the plain text its \
             terminal showed, the latest at the end; it includes interface text, so read it \
             for what was asked and done:\n",
        );
        if kept.chars().count() < total {
            body.push_str("(earlier text omitted)\n");
        }
        body.push_str(&kept);
        body.push('\n');
    }
    let mut prompt = format!("{head}{body}{tail}");
    if prompt.chars().count() > MAX_PROMPT_CHARS {
        // A single message larger than the budget: keep the beginning.
        prompt = prompt.chars().take(MAX_PROMPT_CHARS).collect();
    }
    prompt
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use uuldana_core::{DecodedEvent, Fidelity, ProviderId, SessionId, Timestamp};

    fn entry(role: Role, text: &str) -> TranscriptEntry {
        TranscriptEntry {
            role,
            text: text.into(),
            fidelity: Fidelity::Inferred,
        }
    }

    fn package(entries: Vec<TranscriptEntry>) -> HandoffPackage {
        let mut package = HandoffPackage::new(PathBuf::from("/proyek"));
        package.objective = Some("tambahkan validasi email".into());
        package.transcript = entries;
        package
    }

    /// What the orchestrator tells the next agent about why the work moved. If
    /// the next agent shows its prompt on screen, none of it may read as a usage
    /// limit, or that agent would be taken for one that has run out as well.
    #[test]
    fn no_handoff_reads_as_a_usage_limit_to_the_agent_that_receives_it() {
        use crate::universal_limit::{candidates, normalize};
        for reason in [
            "it could no longer be used on this account",
            "the user chose to switch agents",
            "it could not be resumed",
            "the session was interrupted",
        ] {
            let prompt = render_with_screen(
                &package(vec![
                    entry(Role::User, "tambahkan validasi email"),
                    entry(Role::Agent, "Sudah saya tambahkan di form."),
                ]),
                "account-work",
                reason,
                Some("User: tambahkan validasi email\nAgent: selesai"),
            );
            assert!(
                candidates(&normalize(prompt.as_bytes()))
                    .iter()
                    .all(|c| !c.is_stop()),
                "{reason}\n{prompt}"
            );
        }
    }

    #[test]
    fn prompt_carries_visible_context_and_names_what_is_not_carried() {
        let prompt = render(
            &package(vec![
                entry(Role::User, "tambahkan validasi email"),
                entry(Role::Agent, "Sudah saya tambahkan di form."),
            ]),
            "account-work",
            "its usage limit was reached",
        );
        assert!(prompt.contains("account-work"));
        assert!(prompt.contains("its usage limit was reached"));
        assert!(prompt.contains("Original request:\ntambahkan validasi email"));
        assert!(prompt.contains("User: tambahkan validasi email"));
        assert!(prompt.contains("Agent: Sudah saya tambahkan di form."));
        assert!(prompt.contains("Not carried over:"));
        assert!(prompt.contains("approvals granted in the previous agent"));
        assert!(!prompt.starts_with('-'));
    }

    #[test]
    fn oversized_conversations_keep_the_latest_messages() {
        let big = "x".repeat(5_000);
        let entries: Vec<_> = (0..10)
            .map(|i| entry(Role::User, &format!("pesan-{i} {big}")))
            .collect();
        let prompt = render(&package(entries), "a", "limit");
        assert!(prompt.chars().count() <= MAX_PROMPT_CHARS);
        assert!(prompt.contains("pesan-9"));
        assert!(!prompt.contains("pesan-0"));
        assert!(prompt.contains("earlier messages omitted"));
    }

    #[test]
    fn an_empty_conversation_still_gives_a_usable_prompt() {
        let mut empty = HandoffPackage::new(PathBuf::from("/proyek"));
        empty.objective = None;
        let prompt = render(&empty, "a", "limit");
        assert!(prompt.contains("Continue from where it left off"));
        assert!(!prompt.contains("Visible conversation"));
    }

    #[test]
    fn the_screen_text_is_used_only_when_there_is_no_conversation() {
        let screen = "Catat dulu: nomor kasus 7731\nOke.";
        let without = render_with_screen(
            &package(Vec::new()),
            "agent-c",
            "its usage limit",
            Some(screen),
        );
        assert!(
            without.contains("own history could not be read"),
            "{without}"
        );
        assert!(
            without.contains("Catat dulu: nomor kasus 7731"),
            "{without}"
        );
        assert!(without.contains("Not carried over:"), "{without}");
        // A conversation that could be read wins; the screen is left out.
        let with = render_with_screen(
            &package(vec![entry(Role::User, "perbaiki tes login")]),
            "agent-a",
            "its usage limit",
            Some(screen),
        );
        assert!(with.contains("Visible conversation so far"), "{with}");
        assert!(!with.contains("nomor kasus 7731"), "{with}");
        // Nothing to show, nothing added.
        for none in [None, Some(""), Some("  \n ")] {
            let text = render_with_screen(&package(Vec::new()), "agent-c", "x", none);
            assert!(!text.contains("terminal showed"), "{text}");
        }
    }

    #[test]
    fn a_long_screen_keeps_its_end_and_the_prompt_stays_within_the_limit() {
        let screen: String = (0..6000).map(|n| format!("baris {n}\n")).collect();
        let text = render_with_screen(&package(Vec::new()), "agent-c", "x", Some(&screen));
        assert!(text.chars().count() <= MAX_PROMPT_CHARS);
        assert!(text.contains("baris 5999"));
        assert!(!text.contains("baris 0\n"));
        assert!(text.contains("(earlier text omitted)"));
    }

    #[test]
    fn events_join_chunks_by_author_and_skip_other_runs() {
        let provider = ProviderId::new("acp.fixture").unwrap();
        let session = SessionId::new("s").unwrap();
        let run = RunId::new("run-1").unwrap();
        let other = RunId::new("run-2").unwrap();
        let chunk = |run: &RunId, role, text: &str, seq| {
            EventEnvelope::new(
                seq,
                Timestamp::from_unix_millis(seq),
                session.clone(),
                run.clone(),
                provider.clone(),
                DecodedEvent::native(AgentEvent::MessageChunk {
                    role,
                    text: text.into(),
                }),
            )
        };
        let events = vec![
            chunk(&run, Role::User, "perbaiki ", 0),
            chunk(&run, Role::User, "tes login", 1),
            chunk(&other, Role::Agent, "bukan saya", 2),
            chunk(&run, Role::Agent, "Selesai.", 3),
            chunk(&run, Role::Agent, "   ", 4),
        ];
        let transcript = transcript_from_events(&events, &run);
        let texts: Vec<_> = transcript
            .iter()
            .map(|e| (e.role, e.text.as_str()))
            .collect();
        assert_eq!(
            texts,
            [
                (Role::User, "perbaiki tes login"),
                (Role::Agent, "Selesai.")
            ]
        );
    }

    #[test]
    fn the_screen_recorded_for_a_run_is_read_back_and_the_latest_wins() {
        let session = SessionId::new("s-1").unwrap();
        let provider = ProviderId::new("agent-a").unwrap();
        let run = RunId::new("run-1").unwrap();
        let other = RunId::new("run-2").unwrap();
        let screen = |run: &RunId, text: &str, seq| {
            EventEnvelope::new(
                seq,
                Timestamp::from_unix_millis(seq),
                session.clone(),
                run.clone(),
                provider.clone(),
                DecodedEvent::synthetic(AgentEvent::Extension {
                    namespace: crate::supervisor::SCREEN_NAMESPACE.into(),
                    name: crate::supervisor::SCREEN_EVENT.into(),
                    payload: text.into(),
                }),
            )
        };
        let events = vec![
            screen(&run, "first screen", 1),
            screen(&other, "another run", 2),
            screen(&run, "second screen", 3),
        ];
        assert_eq!(
            screen_from_events(&events, &run).as_deref(),
            Some("second screen")
        );
        assert_eq!(
            screen_from_events(&events, &other).as_deref(),
            Some("another run")
        );
        assert_eq!(
            screen_from_events(&events, &RunId::new("run-3").unwrap()),
            None
        );
        let blank = vec![screen(
            &run, "  
 ", 1,
        )];
        assert_eq!(screen_from_events(&blank, &run), None);
    }
}
