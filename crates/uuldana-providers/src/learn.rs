//! Learning how to drive a program from its own `--help`.
//!
//! Nothing here names a program. A program is taken for a coding agent when
//! its help describes the things an agent has: a way to start with a prompt, a
//! way to resume earlier work, and either permission controls or a model
//! choice. How to start it, resume it and set its permissions is read from the
//! flags and subcommands the help lists. What the help does not say stays
//! unknown and is reported as such, not guessed.

use serde_json::{Value as Json, json};
use uuldana_provider_sdk::{
    ApprovalMode, FilesystemAccess, NetworkAccess, PermissionPolicy, SandboxPolicy,
};

use crate::help::{Help, HelpOption};

/// Words that name the first message of a conversation.
const PROMPT_NAMES: [&str; 9] = [
    "prompt",
    "input",
    "message",
    "question",
    "query",
    "text",
    "task",
    "instruction",
    "request",
];

/// How to hand over the first prompt.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum PromptForm {
    /// As the value of an option that starts an interactive session with it.
    Option(String),
    /// As a positional argument.
    Positional,
}

/// One way to run the program with a given amount of freedom.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct LearnedMode {
    pub effective: PermissionPolicy,
    pub args: Vec<String>,
    pub note: String,
}

/// What was learned about a program.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Profile {
    /// Words that come first on every start, for a program that needs a
    /// subcommand.
    pub leading: Vec<String>,
    pub prompt: PromptForm,
    /// Resume a given session: template with `{session}`.
    pub resume_id: Option<Vec<String>>,
    /// Continue the latest conversation of the folder.
    pub resume_latest: Option<Vec<String>>,
    /// Choose the session id at start: template with `{session}`.
    pub session_id: Option<Vec<String>>,
    pub modes: Vec<LearnedMode>,
    /// Why the program counts as an agent, for display.
    pub reasons: Vec<String>,
    /// What had to be assumed because the help did not say.
    pub assumptions: Vec<String>,
}

const fn policy(approval: ApprovalMode, filesystem: FilesystemAccess) -> PermissionPolicy {
    PermissionPolicy {
        approval,
        sandbox: SandboxPolicy {
            filesystem,
            network: NetworkAccess::Allowed,
        },
    }
}

fn squash(text: &str) -> String {
    text.chars()
        .filter(|c| c.is_alphanumeric())
        .flat_map(char::to_lowercase)
        .collect()
}

fn find(help: &Help, accept: impl Fn(&str) -> bool) -> Option<&HelpOption> {
    help.options
        .iter()
        .find(|option| option.longs().any(|name| accept(&name)))
}

fn find_where(help: &Help, accept: impl Fn(&HelpOption) -> bool) -> Option<&HelpOption> {
    help.options.iter().find(|option| accept(option))
}

fn spelling_args(option: &HelpOption, value: Option<&str>) -> Vec<String> {
    let mut args = vec![option.spelling().to_owned()];
    if let Some(value) = value {
        args.push(value.to_owned());
    }
    args
}

/// An option that starts an interactive session with a given prompt, or else a
/// positional argument that takes it. An option meant for non-interactive
/// output is not one.
fn prompt_form(help: &Help) -> Option<(PromptForm, String)> {
    if let Some(option) = find(help, |name| {
        name.contains("prompt") && name.contains("interactive")
    }) && option.takes_value()
    {
        return Some((
            PromptForm::Option(option.spelling().to_owned()),
            format!("first prompt through {}", option.spelling()),
        ));
    }
    help.positionals
        .iter()
        .find(|name| {
            let name = name.trim_matches(|c: char| !c.is_alphanumeric());
            PROMPT_NAMES.contains(&name)
        })
        .map(|name| (PromptForm::Positional, format!("first prompt as `{name}`")))
}

/// A session id given by the program's own option, preferring the most
/// specific spelling.
fn resume_by_option(help: &Help) -> Option<Vec<String>> {
    for wanted in ["resume-id", "resume", "session", "conversation"] {
        if let Some(option) = find(help, |name| name == wanted)
            && option.takes_value()
        {
            return Some(spelling_args(option, Some("{session}")));
        }
    }
    None
}

/// A switch that continues the most recent conversation of the folder.
fn continue_by_option(help: &Help) -> Option<Vec<String>> {
    for wanted in ["continue", "last", "latest", "resume"] {
        if let Some(option) = find(help, |name| name == wanted)
            && !option.takes_value()
        {
            return Some(spelling_args(option, None));
        }
    }
    None
}

fn session_id_option(help: &Help) -> Option<Vec<String>> {
    find(help, |name| name == "session-id")
        .filter(|option| option.needs_value())
        .map(|option| spelling_args(option, Some("{session}")))
}

/// The switch that skips every permission check.
fn skip_everything(help: &Help) -> Option<&HelpOption> {
    help.options.iter().find(|option| {
        !option.takes_value()
            && option.longs().any(|name| {
                !name.starts_with("allow-")
                    && (name.contains("dangerously")
                        || name.contains("yolo")
                        || name.contains("trust-all")
                        || (name.contains("bypass")
                            && (name.contains("permission") || name.contains("approval"))))
            })
    })
}

/// Reads the word of a permission mode into what it allows.
fn mode_of(choice: &str) -> Option<(ApprovalMode, FilesystemAccess)> {
    let word = squash(choice);
    if word.contains("bypass") || word.contains("yolo") || word.contains("dangerous") {
        Some((ApprovalMode::NeverAsk, FilesystemAccess::Unrestricted))
    } else if word.contains("plan") || word.contains("readonly") {
        Some((ApprovalMode::AskEveryAction, FilesystemAccess::ReadOnly))
    } else if word.contains("acceptedits") || word.contains("autoedit") {
        Some((ApprovalMode::AskForRisky, FilesystemAccess::Unrestricted))
    } else if matches!(
        word.as_str(),
        "default" | "manual" | "ask" | "normal" | "prompt"
    ) {
        Some((ApprovalMode::AskEveryAction, FilesystemAccess::Unrestricted))
    } else {
        None
    }
}

fn filesystem_of(choice: &str) -> Option<FilesystemAccess> {
    let word = squash(choice);
    if word.contains("readonly") {
        Some(FilesystemAccess::ReadOnly)
    } else if word.contains("workspace") {
        Some(FilesystemAccess::WorkspaceWrite)
    } else if word.contains("danger") || word.contains("full") || word.contains("unrestricted") {
        Some(FilesystemAccess::Unrestricted)
    } else {
        None
    }
}

fn approval_of(choice: &str) -> Option<ApprovalMode> {
    let word = squash(choice);
    if word == "never" {
        Some(ApprovalMode::NeverAsk)
    } else if word.contains("onrequest") || word.contains("onfailure") {
        Some(ApprovalMode::AskForRisky)
    } else if word.contains("untrusted") || word == "always" || word == "ask" {
        Some(ApprovalMode::AskEveryAction)
    } else {
        None
    }
}

fn rank(policy: &PermissionPolicy) -> u8 {
    let approval = match policy.approval {
        ApprovalMode::NeverAsk => 2,
        ApprovalMode::AskForRisky => 1,
        _ => 0,
    };
    let filesystem = match policy.sandbox.filesystem {
        FilesystemAccess::Unrestricted => 2,
        FilesystemAccess::WorkspaceWrite => 1,
        _ => 0,
    };
    approval + filesystem
}

/// Every way the help offers to set permissions, most permissive first, and
/// the program's own default last.
fn modes_of(help: &Help, assumptions: &mut Vec<String>) -> Vec<LearnedMode> {
    let mut modes = Vec::new();
    if let Some(option) = skip_everything(help) {
        modes.push(LearnedMode {
            effective: PermissionPolicy::unrestricted(),
            args: spelling_args(option, None),
            note: format!("{} skips every permission check", option.spelling()),
        });
    }
    let sandbox = find_where(help, |o| {
        !o.choices.is_empty() && o.longs().any(|name| name.contains("sandbox"))
    });
    let approval = find_where(help, |o| {
        !o.choices.is_empty()
            && o.takes_value()
            && o.longs().any(|name| {
                (name.contains("approval") || name.contains("ask-for")) && !name.contains("bypass")
            })
    });
    if let (Some(sandbox), Some(approval)) = (sandbox, approval) {
        for fs in &sandbox.choices {
            for ask in &approval.choices {
                if let (Some(filesystem), Some(approval_mode)) =
                    (filesystem_of(fs), approval_of(ask))
                {
                    modes.push(LearnedMode {
                        effective: policy(approval_mode, filesystem),
                        args: vec![
                            sandbox.spelling().to_owned(),
                            fs.clone(),
                            approval.spelling().to_owned(),
                            ask.clone(),
                        ],
                        note: format!(
                            "{} {fs} with {} {ask}",
                            sandbox.spelling(),
                            approval.spelling()
                        ),
                    });
                }
            }
        }
    } else if let Some(option) = find_where(help, |o| {
        o.takes_value()
            && !o.choices.is_empty()
            && o.longs().any(|name| {
                (name.contains("permission") || name.contains("approval")) && !name.contains("skip")
            })
    }) {
        for choice in &option.choices {
            if let Some((approval_mode, filesystem)) = mode_of(choice)
                && !(approval_mode == ApprovalMode::NeverAsk
                    && modes
                        .iter()
                        .any(|m| m.effective == PermissionPolicy::unrestricted()))
            {
                modes.push(LearnedMode {
                    effective: policy(approval_mode, filesystem),
                    args: spelling_args(option, Some(choice)),
                    note: format!("{} {choice}", option.spelling()),
                });
            }
        }
    }
    if modes.is_empty() {
        assumptions.push("the help lists no permission controls".to_owned());
    }
    // Explicit flags first among equals, most freedom first.
    modes.sort_by_key(|mode| std::cmp::Reverse(rank(&mode.effective)));
    modes.dedup_by(|b, a| a.effective == b.effective);
    modes.push(LearnedMode {
        effective: policy(ApprovalMode::AskEveryAction, FilesystemAccess::Unrestricted),
        args: Vec::new(),
        note: "the program's own default, assumed to ask before acting".to_owned(),
    });
    assumptions
        .push("without permission flags the program is assumed to ask before acting".to_owned());
    modes
}

/// What one help text offers, for deciding between bases.
struct Facts {
    prompt: Option<(PromptForm, String)>,
    resume_id: Option<Vec<String>>,
    resume_latest: Option<Vec<String>>,
    permission: bool,
    model: bool,
}

fn facts(help: &Help) -> Facts {
    let permission = skip_everything(help).is_some()
        || find(help, |name| {
            name.contains("permission") || name.contains("approval") || name.contains("sandbox")
        })
        .is_some();
    Facts {
        prompt: prompt_form(help),
        resume_id: resume_by_option(help),
        resume_latest: continue_by_option(help),
        permission,
        model: find(help, |name| name.contains("model")).is_some(),
    }
}

fn worth(facts: &Facts) -> usize {
    usize::from(facts.prompt.is_some())
        + usize::from(facts.resume_id.is_some() || facts.resume_latest.is_some())
        + usize::from(facts.permission)
        + usize::from(facts.model)
}

/// Words that say a subcommand works on an earlier conversation: it continues
/// one, restores one, or copies one. Such a subcommand opens a picker or goes
/// back to what was there, so a new conversation does not start in it.
const EARLIER: [&str; 6] = [
    "resume", "previous", "continue", "earlier", "fork", "restore",
];

fn is_about_an_earlier_conversation(top: &Help, name: &str) -> bool {
    let name = name.to_lowercase();
    if EARLIER.contains(&name.as_str()) {
        return true;
    }
    top.commands
        .iter()
        .find(|command| command.name.to_lowercase() == name)
        .is_some_and(|command| {
            let text = command.text.to_lowercase();
            EARLIER.iter().any(|word| text.contains(word))
        })
}

/// Subcommands worth reading when the top level does not say enough: the ones
/// about resuming, or that describe an interactive conversation. At most `limit`.
pub(crate) fn subcommands_to_read(top: &Help, limit: usize) -> Vec<String> {
    let mut chosen: Vec<(u8, String)> = Vec::new();
    for command in &top.commands {
        let name = command.name.to_lowercase();
        let text = command.text.to_lowercase();
        let rank = if name == "resume" || name == "continue" {
            0
        } else if is_about_an_earlier_conversation(top, &name) {
            continue;
        } else if text.contains("assistant")
            || text.contains("interactive") && !text.contains("non-interactive")
            || text.contains("conversation")
            || name == "chat"
        {
            1
        } else {
            continue;
        };
        chosen.push((rank, command.name.clone()));
    }
    chosen.sort();
    chosen
        .into_iter()
        .map(|(_, name)| name)
        .take(limit)
        .collect()
}

/// Whether a top-level help already says enough that no subcommand is needed.
pub(crate) fn top_is_enough(top: &Help) -> bool {
    let facts = facts(top);
    facts.prompt.is_some() && (facts.resume_id.is_some() || facts.resume_latest.is_some())
}

/// Learns a profile from the top-level help and the help of some subcommands.
/// `None` when the program does not look like a coding agent.
pub(crate) fn learn(top: &Help, subs: &[(String, Help)]) -> Option<Profile> {
    // The base is where a session starts: the top level, or the subcommand
    // that offers the most (a program whose conversation lives in `chat`).
    let top_facts = facts(top);
    let mut base_name: Option<&str> = None;
    let mut base_help = top;
    let mut base_facts = top_facts;
    for (name, help) in subs {
        let candidate = facts(help);
        if !is_about_an_earlier_conversation(top, name) && worth(&candidate) > worth(&base_facts) {
            base_name = Some(name);
            base_help = help;
            base_facts = candidate;
        }
    }
    let (prompt, prompt_reason) = base_facts.prompt.clone()?;
    let mut reasons = vec![prompt_reason];
    let mut resume_id = base_facts.resume_id.clone();
    let mut resume_latest = base_facts.resume_latest.clone();
    // A program that resumes through a subcommand of its own.
    if resume_id.is_none() || resume_latest.is_none() {
        for (name, help) in subs {
            if name != "resume" && name != "continue" {
                continue;
            }
            if resume_id.is_none()
                && help
                    .positionals
                    .iter()
                    .any(|p| p.contains("session") || p == "id" || p.contains("thread"))
            {
                resume_id = Some(vec![name.clone(), "{session}".to_owned()]);
            }
            if resume_latest.is_none()
                && let Some(option) = find(help, |n| n == "last" || n == "latest")
                && !option.takes_value()
            {
                resume_latest = Some(vec![name.clone(), option.spelling().to_owned()]);
            }
        }
    }
    if resume_id.is_none() && resume_latest.is_none() {
        return None;
    }
    if !(base_facts.permission || base_facts.model || facts(top).permission) {
        return None;
    }
    if let Some(args) = &resume_id {
        reasons.push(format!("resume by id: {}", args.join(" ")));
    }
    if let Some(args) = &resume_latest {
        reasons.push(format!("continue the latest: {}", args.join(" ")));
    }
    if base_facts.model {
        reasons.push("has a model option".to_owned());
    }
    let mut assumptions = Vec::new();
    let modes = modes_of(base_help, &mut assumptions);
    if base_facts.permission {
        reasons.push(format!(
            "{} permission mode(s) from its flags",
            modes.len() - 1
        ));
    }
    Some(Profile {
        leading: base_name
            .map(|name| vec![name.to_owned()])
            .unwrap_or_default(),
        prompt,
        resume_id,
        resume_latest,
        session_id: session_id_option(base_help),
        modes,
        reasons,
        assumptions,
    })
}

fn approval_text(mode: ApprovalMode) -> &'static str {
    match mode {
        ApprovalMode::NeverAsk => "never_ask",
        ApprovalMode::AskForRisky => "ask_for_risky",
        _ => "ask_every_action",
    }
}

fn filesystem_text(access: FilesystemAccess) -> &'static str {
    match access {
        FilesystemAccess::Unrestricted => "unrestricted",
        FilesystemAccess::WorkspaceWrite => "workspace_write",
        _ => "read_only",
    }
}

impl Profile {
    /// The profile written as a provider manifest, for the one adapter that
    /// reads manifests.
    pub fn manifest(&self, id: &str, executable: &str) -> Json {
        let modes: Vec<Json> = self
            .modes
            .iter()
            .map(|mode| {
                json!({
                    "approval": approval_text(mode.effective.approval),
                    "filesystem": filesystem_text(mode.effective.sandbox.filesystem),
                    "network": "allowed",
                    "args": mode.args,
                    "note": mode.note,
                })
            })
            .collect();
        let mut manifest = json!({
            "version": 1,
            "id": id,
            "name": id,
            "executables": [executable],
            "modes": modes,
            "interrupts": 2,
            // Agents save their state on the way out, and some take their time.
            "grace_seconds": 30,
        });
        let object = manifest.as_object_mut().expect("an object");
        if !self.leading.is_empty() {
            object.insert("leading_args".into(), json!(self.leading));
        }
        object.insert(
            "prompt_args".into(),
            match &self.prompt {
                PromptForm::Option(flag) => json!([flag, "{prompt}"]),
                PromptForm::Positional => json!(["{prompt}"]),
            },
        );
        if let Some(args) = &self.resume_id {
            object.insert("resume_args".into(), json!(args));
        }
        if let Some(args) = &self.resume_latest {
            object.insert("continue_args".into(), json!(args));
        }
        if let Some(args) = &self.session_id {
            object.insert("session_id_args".into(), json!(args));
        }
        manifest
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::help::parse;

    const A: &str = include_str!("../tests/fixtures/help/a-top.txt");
    const B_TOP: &str = include_str!("../tests/fixtures/help/b-top.txt");
    const B_RESUME: &str = include_str!("../tests/fixtures/help/b-resume.txt");
    const C_TOP: &str = include_str!("../tests/fixtures/help/c-top.txt");
    const C_CHAT: &str = include_str!("../tests/fixtures/help/c-chat.txt");
    const D: &str = include_str!("../tests/fixtures/help/d-top.txt");

    fn only_top(text: &str) -> Option<Profile> {
        learn(&parse(text), &[])
    }

    fn effective(profile: &Profile, wanted: PermissionPolicy) -> (PermissionPolicy, Vec<String>) {
        let mode = profile
            .modes
            .iter()
            .find(|mode| mode.effective.is_within(&wanted))
            .expect("a mode fits");
        (mode.effective, mode.args.clone())
    }

    fn risky_unrestricted() -> PermissionPolicy {
        policy(ApprovalMode::AskForRisky, FilesystemAccess::Unrestricted)
    }

    #[test]
    fn a_program_with_prompt_resume_and_permissions_is_learned_from_its_top_level() {
        let profile = only_top(A).expect("an agent");
        assert_eq!(profile.leading, Vec::<String>::new());
        assert_eq!(profile.prompt, PromptForm::Positional);
        assert_eq!(
            profile.resume_id,
            Some(vec!["--resume".into(), "{session}".into()])
        );
        assert_eq!(profile.resume_latest, Some(vec!["--continue".into()]));
        assert_eq!(
            profile.session_id,
            Some(vec!["--session-id".into(), "{session}".into()])
        );
        let (policy, args) = effective(&profile, risky_unrestricted());
        assert_eq!(policy, risky_unrestricted());
        assert_eq!(args, ["--permission-mode", "acceptEdits"]);
        let (_, args) = effective(&profile, PermissionPolicy::unrestricted());
        assert_eq!(args, ["--dangerously-skip-permissions"]);
        let (read_only, args) = effective(
            &profile,
            policy_of(ApprovalMode::AskEveryAction, FilesystemAccess::ReadOnly),
        );
        assert_eq!(read_only.sandbox.filesystem, FilesystemAccess::ReadOnly);
        assert_eq!(args, ["--permission-mode", "plan"]);
    }

    fn policy_of(approval: ApprovalMode, filesystem: FilesystemAccess) -> PermissionPolicy {
        policy(approval, filesystem)
    }

    #[test]
    fn a_program_that_resumes_through_a_subcommand_is_learned_from_it() {
        let top = parse(B_TOP);
        assert!(!top_is_enough(&top));
        let names = subcommands_to_read(&top, 6);
        assert_eq!(names.first().map(String::as_str), Some("resume"));
        let profile = learn(&top, &[("resume".to_owned(), parse(B_RESUME))]).expect("an agent");
        assert_eq!(profile.prompt, PromptForm::Positional);
        assert_eq!(
            profile.resume_id,
            Some(vec!["resume".into(), "{session}".into()])
        );
        assert_eq!(
            profile.resume_latest,
            Some(vec!["resume".into(), "--last".into()])
        );
        let (policy, args) = effective(&profile, risky_unrestricted());
        assert_eq!(policy.approval, ApprovalMode::AskForRisky);
        assert!(args.contains(&"on-request".to_owned()), "{args:?}");
        assert!(args.contains(&"danger-full-access".to_owned()), "{args:?}");
        let (policy, args) = effective(
            &profile,
            policy_of(ApprovalMode::AskForRisky, FilesystemAccess::WorkspaceWrite),
        );
        assert_eq!(policy.sandbox.filesystem, FilesystemAccess::WorkspaceWrite);
        assert!(args.contains(&"workspace-write".to_owned()), "{args:?}");
    }

    #[test]
    fn a_subcommand_that_forks_an_earlier_session_is_not_where_a_session_starts() {
        const B_FORK: &str = include_str!("../tests/fixtures/help/b-fork.txt");
        let top = parse(B_TOP);
        // The fork help looks like an agent's: a prompt, a model, a sandbox and
        // a "last" switch. It still only works on an earlier session.
        assert!(is_about_an_earlier_conversation(&top, "fork"));
        assert!(!subcommands_to_read(&top, 8).contains(&"fork".to_owned()));
        let subs = vec![
            ("resume".to_owned(), parse(B_RESUME)),
            ("fork".to_owned(), parse(B_FORK)),
        ];
        let profile = learn(&top, &subs).expect("an agent");
        assert_eq!(profile.leading, Vec::<String>::new());
        assert_eq!(
            profile.resume_latest,
            Some(vec!["resume".into(), "--last".into()])
        );
        // The subcommand that does start a conversation is not caught.
        let chat = learn(&parse(C_TOP), &[("chat".to_owned(), parse(C_CHAT))]).expect("an agent");
        assert_eq!(chat.leading, ["chat"]);
    }

    #[test]
    fn a_program_whose_conversation_lives_in_a_subcommand_leads_with_it() {
        let top = parse(C_TOP);
        assert!(!top_is_enough(&top));
        assert_eq!(subcommands_to_read(&top, 6), ["chat"]);
        let profile = learn(&top, &[("chat".to_owned(), parse(C_CHAT))]).expect("an agent");
        assert_eq!(profile.leading, ["chat"]);
        assert_eq!(profile.prompt, PromptForm::Positional);
        assert_eq!(
            profile.resume_id,
            Some(vec!["--resume-id".into(), "{session}".into()])
        );
        assert_eq!(profile.resume_latest, Some(vec!["--resume".into()]));
        assert_eq!(profile.session_id, None);
        // Its only permission flag trusts every tool; otherwise its own default.
        let (_, args) = effective(&profile, PermissionPolicy::unrestricted());
        assert_eq!(args, ["--trust-all-tools"]);
        let (policy, args) = effective(&profile, risky_unrestricted());
        assert_eq!(policy.approval, ApprovalMode::AskEveryAction);
        assert!(args.is_empty());
        assert!(!profile.assumptions.is_empty());
    }

    #[test]
    fn an_interactive_prompt_option_is_used_and_unknown_modes_are_left_out() {
        let profile = only_top(D).expect("an agent");
        assert_eq!(
            profile.prompt,
            PromptForm::Option("--prompt-interactive".into())
        );
        assert_eq!(profile.resume_latest, Some(vec!["--continue".into()]));
        let (_, args) = effective(&profile, risky_unrestricted());
        assert_eq!(args, ["--permission-mode", "accept_edits"]);
        // `dont_ask` and `auto` say too little to map, so they are not modes.
        assert!(
            !profile
                .modes
                .iter()
                .any(|m| m.args.iter().any(|a| a == "dont_ask" || a == "auto"))
        );
    }

    #[test]
    fn the_manifest_of_a_profile_loads_as_an_adapter() {
        let profile = only_top(A).unwrap();
        let manifest = profile.manifest("tool", "C:/tools/tool.exe");
        let adapter = crate::external::ExternalAdapter::from_value(&manifest, false);
        assert!(adapter.is_ok(), "{:?}", adapter.err());
    }

    #[test]
    fn programs_that_are_not_agents_are_not_taken_for_one() {
        // A build tool.
        let build = "Usage: tool [OPTIONS] <COMMAND>\n\nCommands:\n  build  Compile the project\n  test   Run the tests\n  run    Run a binary\n\nOptions:\n  -v, --verbose  Use verbose output\n  -q, --quiet    Do not print output\n  -h, --help     Print help\n  -V, --version  Print version\n";
        assert!(only_top(build).is_none());
        // A version control tool.
        let vcs = "usage: tool [-v | --version] [-h | --help] <command> [<args>]\n\nstart a working area\n   clone     Clone a repository\n   init      Create an empty repository\n";
        assert!(only_top(vcs).is_none());
        // Text that is not help.
        assert!(only_top("error: unrecognized option\n").is_none());
        assert!(only_top("").is_none());
        // A script runner with a prompt-like option but nothing to resume.
        let runner = "Usage: runner [OPTIONS] [INPUT]\n\nArguments:\n  [INPUT]  What to run\n\nOptions:\n  -m, --model <MODEL>  Model\n  -h, --help  Print help\n";
        assert!(only_top(runner).is_none());
    }
}
