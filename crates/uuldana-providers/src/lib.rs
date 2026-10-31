//! Provider adapters for Uuldana.
//!
//! No agent is known by name. Agents installed on the machine are found on the
//! search path and learned from what they print for `--help` ([`learn_installed`]),
//! and a provider can also be described by a manifest file
//! ([`load_external_adapters`]). An adapter launches the program in its own
//! terminal UI and describes only the program, arguments and working
//! directory. None of them reads credentials, writes provider configuration or
//! replaces the provider's agent loop. Permission mappings report what the
//! program's flags really enforce, so a policy it cannot honor is refused by
//! the core instead of being launched with more permission than requested.

mod common;
mod discover;
mod external;
mod help;
mod learn;
mod learned;
mod limits;

pub use external::{ExternalAdapter, load_dir as load_external_adapters};
pub use learned::{
    LearnedAdapter, learn_installed, learn_installed_with_progress, learn_listed_with_progress,
};

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{help, learn};
    use std::path::PathBuf;
    use uuldana_provider_sdk::{Installation, PermissionPolicy, ProviderAdapter, validate_mapping};

    /// Agents learned from recorded help texts of programs that were installed
    /// when the texts were recorded.
    fn recorded() -> Vec<ExternalAdapter> {
        let cases: [(&str, &[(&str, &str)]); 4] = [
            (include_str!("../tests/fixtures/help/a-top.txt"), &[]),
            (
                include_str!("../tests/fixtures/help/b-top.txt"),
                &[
                    (
                        "resume",
                        include_str!("../tests/fixtures/help/b-resume.txt"),
                    ),
                    ("fork", include_str!("../tests/fixtures/help/b-fork.txt")),
                ],
            ),
            (
                include_str!("../tests/fixtures/help/c-top.txt"),
                &[("chat", include_str!("../tests/fixtures/help/c-chat.txt"))],
            ),
            (include_str!("../tests/fixtures/help/d-top.txt"), &[]),
        ];
        cases
            .iter()
            .enumerate()
            .map(|(at, (top, subs))| {
                let subs: Vec<(String, help::Help)> = subs
                    .iter()
                    .map(|(name, text)| ((*name).to_owned(), help::parse(text)))
                    .collect();
                let profile = learn::learn(&help::parse(top), &subs).expect("an agent");
                let manifest = profile.manifest(&format!("agent-{at}"), "C:/agents/agent.exe");
                ExternalAdapter::from_value(&manifest, false).expect("a manifest")
            })
            .collect()
    }

    /// Which of the recorded agents (by position) can start under `ceiling`.
    fn startable_under(ceiling: &PermissionPolicy) -> Vec<usize> {
        let installation = Installation::new(PathBuf::from("C:/agents/agent.exe"));
        recorded()
            .iter()
            .enumerate()
            .filter(|(_, adapter)| {
                let mapping = adapter.map_permissions(&installation, ceiling);
                validate_mapping(ceiling, &mapping).is_ok()
            })
            .map(|(at, _)| at)
            .collect()
    }

    fn ceiling(
        approval: uuldana_provider_sdk::ApprovalMode,
        filesystem: uuldana_provider_sdk::FilesystemAccess,
    ) -> PermissionPolicy {
        PermissionPolicy {
            approval,
            sandbox: uuldana_provider_sdk::SandboxPolicy {
                filesystem,
                network: uuldana_provider_sdk::NetworkAccess::Allowed,
            },
        }
    }

    #[test]
    fn without_any_permissions_every_learned_agent_can_start() {
        // The ceiling used when there is no configuration at all.
        let automatic = uuldana_runtime::config::automatic_ceiling();
        assert_eq!(startable_under(&automatic), [0, 1, 2, 3]);
        assert_eq!(
            startable_under(&PermissionPolicy::unrestricted()),
            [0, 1, 2, 3]
        );
    }

    #[test]
    fn a_tighter_ceiling_starts_only_the_agents_with_a_mode_inside_it() {
        use uuldana_provider_sdk::{ApprovalMode::AskEveryAction, FilesystemAccess};
        // Only one of the four recorded agents lists a mode that asks about
        // everything and writes nothing (its help names a plan mode). One
        // lists approval choices of "on request" and "never" only, and the
        // other two have nothing but their default way of running.
        assert_eq!(
            startable_under(&ceiling(AskEveryAction, FilesystemAccess::ReadOnly)),
            [0]
        );
        assert_eq!(
            startable_under(&ceiling(AskEveryAction, FilesystemAccess::WorkspaceWrite)),
            [0]
        );
    }

    #[test]
    fn the_strictest_ceiling_starts_no_agent_because_none_can_block_the_network() {
        // Control for the test above: the automatic ceiling is what makes the
        // difference.
        assert!(startable_under(&PermissionPolicy::most_restrictive()).is_empty());
    }
}
