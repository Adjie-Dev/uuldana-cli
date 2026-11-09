# Security policy

## Reporting a vulnerability

Please do not open a public issue for a security problem. Report it privately by email to aniccacodestudio@gmail.com, with steps to reproduce, the affected version and the platform. You will get an acknowledgement, and a fix or an explanation, before details are made public.

The email address is the maintainer's own; the project has no hosting page yet, so there is no other private channel.

## What Uuldana does with your data

- **Credentials.** Uuldana never reads, copies or logs provider credentials. Provider adapters are required not to open token or key files, and environment values from your configuration are redacted in debug output.
- **Provider configuration.** Uuldana does not write to a provider's configuration, plugin or settings files, and reads none of an agent's own files.
- **Finding agents runs programs.** With detection on (the default), the first start runs every console program on your search path (outside the operating system's own folders) with `--help`, and a few with a subcommand and `--version`. Stdin is closed, each gets five seconds, and a program is asked once; the answers are kept in the data directory until the program changes. A program that ignores `--help` and does something else will do it. The programs get a reduced environment: the system folders, locale and terminal settings only, so API keys and tokens in your environment are not passed to them. They run in an empty temporary folder of their own, removed afterwards, not in the folder you started Uuldana in. If you do not want unknown programs run, set `"auto_detect": false` in `config.json` and list your agents in `providers`; then only the listed programs are ever run. The probe path has not been run on Linux or macOS.
- **Journals.** Session journals under the Uuldana data directory contain the events Uuldana observed, and, when a run ends, the text the agent's screen showed (the last 120 lines with text), which is visible conversation text. On Linux and macOS new journals, `handoff.md` and the files Uuldana writes to `config.json` are created readable by their owner only (modes 0600 and 0700). On Windows they take the access of your user profile folder. Anything already on disk keeps the access it had. Delete a journal to delete that history; Uuldana keeps the 200 most recent and deletes older ones.
- **Network.** Uuldana makes no network requests of its own. Providers do, as they normally would.

## Limits to know about

- Permission mapping describes what each provider's own flags enforce. It is not an operating system sandbox, and an approved action in a provider can touch anything that provider can touch.
- Adapters are trusted code. A compiled adapter can do anything the process can; review third-party adapters before building them in. A provider manifest is data, but it names an executable that Uuldana will launch, so only install manifests you trust.
- A handoff carries the text an agent's screen showed into the next agent as its first prompt. Instructions hidden in that text (from a web page the first agent read, say) travel with it, as they would if you pasted it yourself. Failover keeps the permission ceiling, but it does not clean the conversation.
- Usage-limit detection reads text a provider prints. A conversation that quotes a limit message could trigger a switch; the detector needs the limit wording together with a reset or retry hint and a quiet provider to reduce this.

## Dependencies

`Cargo.lock` is committed and every build uses `--locked`. CI runs `cargo audit` against the RustSec database, and Dependabot proposes updates. Two crates are vendored with patches (`vendor/portable-pty`, `vendor/subc-jobobject`); each carries a `UULDANA_PATCH.md` that says what changed. Release binaries are not code signed yet.
