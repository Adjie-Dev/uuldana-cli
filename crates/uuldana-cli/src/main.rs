//! `uuldana`: terminal entry point of the Uuldana supervisor.
//!
//! `uuldana` starts the highest priority configured provider in its native
//! terminal UI, `uuldana providers` lists what this machine has, and
//! `uuldana --version` / `--help` print information.

mod banner;
mod frame;
mod session;
mod terminal;

use std::process::ExitCode;

use uuldana_core::CONTRACT_VERSION;

const VERSION: &str = env!("CARGO_PKG_VERSION");

const USAGE: &str = "\
Uuldana: a universal, open-source native AI coding agent supervisor.

Usage:
  uuldana                Start a supervised session in this directory
  uuldana resume         Resume a supervised session
  uuldana providers      List the agents found on this machine
  uuldana --version      Print version information
  uuldana --help         Print this help

Options for start and resume:
  --bypass-permissions   Never ask, no filesystem or network limits, for this
                         run only (overrides the configured ceiling)";

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Command {
    Start,
    Resume,
    Providers,
    Version,
    Help,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct UsageError(String);

/// A parsed command line: the command and whether the permission ceiling is
/// lifted for this run.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Invocation {
    command: Command,
    bypass_permissions: bool,
}

fn parse_args<I>(args: I) -> Result<Invocation, UsageError>
where
    I: IntoIterator<Item = String>,
{
    let mut command = None;
    let mut bypass_permissions = false;
    for arg in args {
        let parsed = match arg.as_str() {
            "--bypass-permissions" => {
                bypass_permissions = true;
                continue;
            }
            "resume" => Command::Resume,
            "providers" => Command::Providers,
            "--version" | "-V" => Command::Version,
            "--help" | "-h" | "help" => Command::Help,
            other => return Err(UsageError(format!("unknown argument: {other}"))),
        };
        if command.is_some() {
            return Err(UsageError(format!("unexpected argument: {arg}")));
        }
        command = Some(parsed);
    }
    let command = command.unwrap_or(Command::Start);
    if bypass_permissions && !matches!(command, Command::Start | Command::Resume) {
        return Err(UsageError(
            "--bypass-permissions only applies to starting or resuming a session".into(),
        ));
    }
    Ok(Invocation {
        command,
        bypass_permissions,
    })
}

fn main() -> ExitCode {
    let Invocation {
        command,
        bypass_permissions,
    } = match parse_args(std::env::args().skip(1)) {
        Ok(invocation) => invocation,
        Err(UsageError(message)) => {
            eprintln!("error: {message}\n\n{USAGE}");
            return ExitCode::from(2);
        }
    };
    match command {
        Command::Version => {
            println!("uuldana {VERSION} (provider contract {CONTRACT_VERSION})");
            ExitCode::SUCCESS
        }
        Command::Help => {
            println!("{USAGE}");
            ExitCode::SUCCESS
        }
        Command::Providers => {
            print!("{}", session::list_providers());
            ExitCode::SUCCESS
        }
        Command::Start => session::start(bypass_permissions),
        Command::Resume => session::resume(bypass_permissions),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(args: &[&str]) -> Result<Command, UsageError> {
        parse_args(args.iter().map(|s| (*s).to_owned())).map(|i| i.command)
    }

    fn bypass(args: &[&str]) -> Result<bool, UsageError> {
        parse_args(args.iter().map(|s| (*s).to_owned())).map(|i| i.bypass_permissions)
    }

    #[test]
    fn parses_known_commands() {
        assert_eq!(parse(&[]), Ok(Command::Start));
        assert_eq!(parse(&["resume"]), Ok(Command::Resume));
        assert_eq!(parse(&["providers"]), Ok(Command::Providers));
        assert_eq!(parse(&["--version"]), Ok(Command::Version));
        assert_eq!(parse(&["-V"]), Ok(Command::Version));
        assert_eq!(parse(&["--help"]), Ok(Command::Help));
    }

    #[test]
    fn rejects_unknown_and_extra_arguments() {
        assert!(parse(&["launch"]).is_err());
        assert!(parse(&["providers", "--all"]).is_err());
        assert!(parse(&["resume", "providers"]).is_err());
    }

    #[test]
    fn bypass_flag_applies_to_start_and_resume_in_any_order() {
        assert_eq!(bypass(&[]), Ok(false));
        assert_eq!(bypass(&["--bypass-permissions"]), Ok(true));
        assert_eq!(
            parse(&["--bypass-permissions"]),
            Ok(Command::Start),
            "the flag alone starts a session"
        );
        assert_eq!(bypass(&["resume", "--bypass-permissions"]), Ok(true));
        assert_eq!(
            parse(&["--bypass-permissions", "resume"]),
            Ok(Command::Resume)
        );
        assert_eq!(
            bypass(&["--bypass-permissions", "--bypass-permissions"]),
            Ok(true)
        );
    }

    #[test]
    fn bypass_flag_is_refused_for_commands_that_start_nothing() {
        assert!(parse(&["providers", "--bypass-permissions"]).is_err());
        assert!(parse(&["--bypass-permissions", "--version"]).is_err());
        assert!(parse(&["--help", "--bypass-permissions"]).is_err());
    }
}
