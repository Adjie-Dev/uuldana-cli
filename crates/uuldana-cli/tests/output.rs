//! CLI output and exit-code regression checks with redirected streams.

use std::process::Command;

/// The `uuldana` binary. On a build server the search path is an empty folder:
/// that machine has no agent, and `uuldana providers` would otherwise ask every
/// program on its search path for `--help`, which takes minutes.
fn uuldana() -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_uuldana"));
    if std::env::var_os("CI").is_some() {
        let empty = std::path::PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("empty-search-path");
        std::fs::create_dir_all(&empty).unwrap();
        command.env("PATH", empty);
    }
    command
}

#[test]
fn redirected_version_preserves_output_and_exit_status() {
    let output = uuldana().arg("--version").output().expect("run CLI");
    assert_eq!(output.status.code(), Some(0));
    assert_eq!(
        String::from_utf8(output.stdout).unwrap(),
        format!(
            "uuldana {} (provider contract 0.2)\n",
            env!("CARGO_PKG_VERSION")
        )
    );
    assert!(output.stderr.is_empty());
}

#[test]
fn providers_lists_what_was_found_in_the_order_of_its_ids_without_decoration() {
    let data = std::env::temp_dir().join(format!("uuldana-providers-data-{}", std::process::id()));
    let output = uuldana()
        .arg("providers")
        .env("UULDANA_DATA", &data)
        .output()
        .expect("run CLI");
    let _ = std::fs::remove_dir_all(&data);
    assert_eq!(output.status.code(), Some(0));
    let text = String::from_utf8(output.stdout).unwrap();
    // The registry lists adapters alphabetically; priority is never implied.
    // What was learned about an agent follows its line, indented.
    let ids: Vec<&str> = text
        .lines()
        .filter(|line| !line.starts_with(' '))
        .map(|line| line.split_whitespace().next().unwrap())
        .collect();
    let mut sorted = ids.clone();
    sorted.sort_unstable();
    assert_eq!(ids, sorted);
    assert!(!text.contains('\u{1b}'));
    // The first look at the machine says so; nothing else goes to stderr.
    let errors = String::from_utf8(output.stderr).unwrap();
    assert!(
        errors.lines().all(|line| line.starts_with("uuldana: ")),
        "{errors}"
    );
}

#[test]
fn start_without_configuration_still_starts_and_is_stopped_only_by_the_terminal() {
    let missing = std::env::temp_dir().join(format!("uuldana-no-config-{}", std::process::id()));
    let output = uuldana()
        .env("UULDANA_CONFIG", &missing)
        .output()
        .expect("run CLI");
    // A missing file is not an error: the run goes on to the check of the
    // terminal, which is all that stops it here.
    assert_eq!(output.status.code(), Some(2));
    assert!(output.stdout.is_empty());
    assert_eq!(
        String::from_utf8(output.stderr).unwrap(),
        "uuldana: a supervised session needs an interactive terminal\n"
    );
}

#[test]
fn resume_follows_the_same_rules_as_start() {
    let missing =
        std::env::temp_dir().join(format!("uuldana-resume-config-{}", std::process::id()));
    let output = uuldana()
        .arg("resume")
        .env("UULDANA_CONFIG", &missing)
        .output()
        .expect("run CLI");
    assert_eq!(output.status.code(), Some(2));
    assert_eq!(
        String::from_utf8(output.stderr).unwrap(),
        "uuldana: a supervised session needs an interactive terminal\n"
    );

    let path = std::env::temp_dir().join(format!("uuldana-resume-{}.json", std::process::id()));
    std::fs::write(&path, r#"{"version":1,"providers":[{"id":"agent-b"}]}"#).unwrap();
    let output = uuldana()
        .arg("resume")
        .env("UULDANA_CONFIG", &path)
        .output()
        .expect("run CLI");
    std::fs::remove_file(&path).unwrap();
    assert_eq!(output.status.code(), Some(2));
    assert_eq!(
        String::from_utf8(output.stderr).unwrap(),
        "uuldana: a supervised session needs an interactive terminal\n"
    );
}

#[test]
fn start_needs_an_interactive_terminal() {
    let path = std::env::temp_dir().join(format!("uuldana-config-{}.json", std::process::id()));
    std::fs::write(&path, r#"{"version":1,"providers":[{"id":"agent-b"}]}"#).unwrap();
    let output = uuldana()
        .env("UULDANA_CONFIG", &path)
        .output()
        .expect("run CLI");
    std::fs::remove_file(&path).unwrap();
    assert_eq!(output.status.code(), Some(2));
    assert_eq!(
        String::from_utf8(output.stderr).unwrap(),
        "uuldana: a supervised session needs an interactive terminal\n"
    );
}

#[test]
fn start_rejects_an_invalid_configuration_precisely() {
    let path = std::env::temp_dir().join(format!("uuldana-bad-config-{}.json", std::process::id()));
    std::fs::write(&path, r#"{"version":1,"auto_detect":false,"providers":[]}"#).unwrap();
    let output = uuldana()
        .env("UULDANA_CONFIG", &path)
        .output()
        .expect("run CLI");
    std::fs::remove_file(&path).unwrap();
    assert_eq!(output.status.code(), Some(2));
    let text = String::from_utf8(output.stderr).unwrap();
    assert!(text.contains("at least one provider"), "{text}");
}

#[test]
fn redirected_help_and_errors_have_no_decoration() {
    for args in [
        vec!["--help"],
        vec!["help"],
        vec!["unknown"],
        vec!["providers", "--json"],
    ] {
        let output = uuldana().args(&args).output().expect("run CLI");
        assert_eq!(
            output.status.code(),
            Some(if args[0] == "--help" || args[0] == "help" {
                0
            } else {
                2
            })
        );
        for stream in [&output.stdout, &output.stderr] {
            let text = String::from_utf8_lossy(stream);
            assert!(!text.contains("ONE TERMINAL."), "{args:?}");
            assert!(!text.contains('\u{1b}'), "{args:?}");
        }
    }
}

#[test]
fn a_provider_manifest_next_to_the_configuration_appears_in_the_listing() {
    let dir = std::env::temp_dir().join(format!("uuldana-adapters-cli-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join("adapters")).unwrap();
    std::fs::write(
        dir.join("adapters").join("my-agent.json"),
        r#"{"version":1,"id":"my-agent","executables":["definitely-not-installed-agent"],
            "modes":[{"approval":"ask_every_action","filesystem":"unrestricted","network":"allowed"}]}"#,
    )
    .unwrap();
    std::fs::write(dir.join("adapters").join("broken.json"), "{").unwrap();
    let output = uuldana()
        .arg("providers")
        .env("UULDANA_CONFIG", dir.join("config.json"))
        .env("UULDANA_DATA", dir.join("data"))
        .output()
        .expect("run CLI");
    std::fs::remove_dir_all(&dir).unwrap();
    assert_eq!(output.status.code(), Some(0));
    let text = String::from_utf8(output.stdout).unwrap();
    assert!(
        text.lines()
            .any(|l| l.starts_with("my-agent") && l.contains("not installed")),
        "{text}"
    );
    let errors = String::from_utf8(output.stderr).unwrap();
    assert!(
        errors.contains("adapter not loaded: broken.json"),
        "{errors}"
    );
}
