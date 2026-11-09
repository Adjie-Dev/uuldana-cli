# Uuldana

[![CI](https://github.com/Adjie-Dev/uuldana-cli/actions/workflows/ci.yml/badge.svg)](https://github.com/Adjie-Dev/uuldana-cli/actions/workflows/ci.yml)
[![npm](https://img.shields.io/npm/v/uuldana-cli)](https://www.npmjs.com/package/uuldana-cli)
[![License: MIT](https://img.shields.io/badge/license-MIT-blue.svg)](LICENSE-MIT)

**Uuldana runs the AI coding agents you already use, each in its own terminal interface, and keeps your work going when one of them runs out of quota.**

It is a supervisor, not another agent. Each agent stays itself: its tools, plugins, skills, MCP servers and sign-ins are the ones you already set up. Uuldana starts them, watches them, remembers what happened and, when the active agent hits a usage limit, continues the work on the next one in the order you chose.

**Website and documentation: [uuldana-cli.pages.dev](https://uuldana-cli.pages.dev)**

> **Early access (0.1.x).** It works and is tested, but it is young. Read [Status and limits](#status-and-limits) before you depend on it.

## The problem it solves

You use more than one coding agent, or more than one account. Each one stops at its usage limit, usually in the middle of a task. Then you quit, open another agent, and explain everything again.

With Uuldana:

```
 you work in agent A  ->  A prints its usage limit  ->  Uuldana stops A gracefully
                                                          |
 you keep working in agent B  <-  B starts with the visible conversation as its first prompt
```

No Ctrl+C, no retyping, no new sign-in.

## Install

### Windows x64 (npm)

You need [Node.js](https://nodejs.org) 18 or newer. It only runs a small launcher; the program itself is a native binary.

```
npm install -g uuldana-cli
uuldana --version
```

The package is called `uuldana-cli`. The command it installs is `uuldana`.

The binary is not code signed yet, so Windows SmartScreen may show a warning the first time.

### Linux and macOS (from source)

There is no npm package for these systems yet. Build with a current stable Rust toolchain (edition 2024):

```
cargo build --release -p uuldana-cli
```

The binary is `target/release/uuldana`. See [Platform support](#platform-support) for what has and has not been tested there. On Windows, building from source needs a complete MinGW-w64 install (GNU toolchain) or the Visual Studio Build Tools (MSVC); see [Windows development](docs/WINDOWS_DEVELOPMENT.md).

## Quick start

1. **Install your agents the way you normally do.** Uuldana has no list of agents and needs no setup.
2. **Open a terminal in your project and run:**
   ```
   uuldana
   ```
   The first start looks for agents, which takes a while (see [What the first start does](#what-the-first-start-does)). After that it starts immediately.
3. **Work as usual.** The agent's own screen runs under a header that shows Uuldana and which agent is working. When it hits a limit, Uuldana moves to the next agent.

Come back to the same work later with:

```
uuldana resume
```

## Commands

| Command | What it does |
| --- | --- |
| `uuldana` | Start a session in this directory with the first available agent |
| `uuldana resume` | Continue the last session of this directory |
| `uuldana providers` | List the agents found on this machine and what was learned about them |
| `uuldana --version` | Print the version |
| `uuldana --help` | Print the usage text |
| `--bypass-permissions` | With `uuldana` or `uuldana resume`: never ask, no filesystem or network limits, for this run only |

## Inside a session

- Type `/agent` and press Enter on an empty line to choose which agent works first and which ones take over after it. See [choosing the order inside a session](docs/CONFIGURATION.md#choosing-the-order-inside-a-session).
- The header always shows Uuldana, the working agent and any warning (for example, permissions bypassed). An agent that clears or repaints the screen cannot remove it.
- Your keyboard goes to the agent unchanged. The terminal follows resizing.
- The mouse wheel scrolls the agent's earlier output; Shift+PageUp and Shift+PageDown do the same, and any other key returns to the live screen. An agent that asks for the mouse itself is sent the wheel instead.
- Uuldana takes the mouse, so to select text, hold Shift while you drag.

## How automatic failover works

1. Uuldana reads what every agent prints. It looks for a message saying an allowance is used up, together with when it returns or what to do (resets, try again, upgrade, credits). A pause for a per-minute rate limit is not a stop.
2. When it sees that, it stops the agent gracefully and rests that entry for a while.
3. It hands the **visible conversation** to the next agent in your order as its first prompt, and keeps going.

This is a heuristic on text. Usage-limit messages are not a documented interface and may change, and an agent that keeps working after such text is left running.

**What a handoff carries, and what it does not:**

- It carries the plain text the agent's screen showed (the last 120 lines with text, interface text included). Uuldana reads none of an agent's own session files.
- It does not move hidden reasoning, memory, approvals or running tool calls.
- When you move the work yourself with `/agent`, the conversation is pasted into the new agent's input box and **not sent**, so nothing is spent until you press Enter. When a usage limit moves it, the new agent starts on it at once, because you are not there to press Enter.
- If nobody has spoken to the agent yet, there is nothing to carry and the next agent starts clean.

## What the first start does

Uuldana keeps no list of agents. At the first start it reads your search path (leaving out the operating system's own folders), takes the console programs it finds, and asks each one for `--help` once. A program whose help describes a coding agent becomes one:

- a way to start with a first prompt;
- a way to resume earlier work (a flag with a session id, a subcommand, or a flag that continues the latest conversation of the folder);
- permission controls, or a model option.

**This means Uuldana runs the programs on your search path with `--help`.** Each runs with stdin closed, five seconds to answer, a reduced environment (no API keys or tokens), and an empty temporary folder of its own. A program that ignores `--help` and does something else will do it. If you do not want unknown programs run, put this in `config.json` and list your agents yourself; then only the listed programs are ever run:

```json
{ "version": 1, "auto_detect": false, "providers": [{ "id": "your-agent" }] }
```

The answers are kept in `programs.json` in the data directory and a program is asked again only after it changes. On the test machine the first look took fifteen to thirty seconds; after that the lookup takes a tenth of a second. A new agent you install is found the next time you start Uuldana. If something was learned wrongly, a [manifest](docs/PROVIDER_SDK.md#provider-manifests) with the same id overrides it.

Permission modes are mapped from the words in the help (a plan or read-only mode asks about everything and writes nothing; an accept-edits mode asks about risky actions; a bypass mode never asks). A program with no permission flag is assumed to ask before acting, and `uuldana providers` marks that as an assumption. What the help does not say is left unknown, not guessed.

## Configuration

Optional. `config.json` is only for choosing the order, adding accounts or setting a permission ceiling.

| Platform | Default location |
| --- | --- |
| Windows | `%APPDATA%\uuldana\config.json` |
| Linux, macOS | `$XDG_CONFIG_HOME/uuldana/config.json`, or `~/.config/uuldana/config.json` |

`UULDANA_CONFIG` overrides it. A minimal file (the ids are examples; an id is the name of the agent's program without the extension, as `uuldana providers` lists it):

```json
{
  "version": 1,
  "permissions": {
    "approval": "ask_for_risky",
    "filesystem": "unrestricted",
    "network": "allowed"
  },
  "providers": [
    { "id": "claude" },
    { "id": "codex" }
  ]
}
```

An entry that matches no agent on the machine is left out, with a notice. The same agent can appear several times, each with its own environment, which is how a second account is selected. Uuldana never reads or copies the credentials behind it. See [configuration](docs/CONFIGURATION.md) for every field, including accounts, cooldowns and provider manifests.

## Permissions that do not creep

You set a ceiling for approval, filesystem and network. A provider that cannot enforce the ceiling with its own flags is refused instead of being launched with more permission. Failover never raises it. `--bypass-permissions` is the only override, for one run, and the header shows a warning while it is on.

## Privacy and safety

- **Credentials.** Uuldana never reads, copies or logs provider credentials.
- **Network.** Uuldana makes no network requests of its own. The agents do, as they normally would.
- **What is stored.** Each session is written to an append-only journal under the data directory (`%LOCALAPPDATA%\uuldana` on Windows, `$XDG_DATA_HOME/uuldana` or `~/.local/share/uuldana` elsewhere). It holds the events Uuldana observed and, when a run ends, the last 120 lines of text the agent's screen showed, which is visible conversation text. On Linux and macOS these files are readable by their owner only; on Windows they take the access of your user profile folder. Uuldana keeps the 200 most recent journals. Delete a journal to delete that history.
- **Handoff text can carry instructions.** Text the first agent read (from a web page, say) travels with the handoff, as it would if you pasted it yourself. Failover keeps the permission ceiling, but it does not clean the conversation.
- **Not a sandbox.** Permission mapping describes what each provider's own flags enforce. An approved action in a provider can touch anything that provider can touch.

The full policy, including how to report a vulnerability, is in [SECURITY.md](SECURITY.md).

## Questions

**Which agents does it support?** Any console agent whose `--help` describes a way to start with a prompt and a way to resume. No agent is known by name. Four agents were tried on real installs on the test machine, each opened through Uuldana and left with Ctrl+C, and stopped gracefully the way a failover stops it. Agents that were not installed there have not been tried.

**Has failover between two real agents been tested?** Not for every agent, because that would burn an account. It is covered with real child processes standing in for agents, and with real usage-limit messages as test inputs.

**Does it translate plugins, skills or MCP servers?** No. A plugin works whenever the agent that owns it is the one running. See [plugin compatibility](docs/PLUGIN_COMPATIBILITY.md).

**Does it send my code anywhere?** Uuldana does not. It makes no network requests. Whatever an agent sends to its own provider is between you and that provider.

**Does it cost anything?** No. It is free and open source under the MIT license. The agents keep their own pricing.

## Platform support

| Platform | Install | Status |
| --- | --- | --- |
| Windows 11 x64 | `npm install -g uuldana-cli` | Where Uuldana is developed and tested (GNU and MSVC toolchains). |
| Linux | From source | The test suite passes on Ubuntu in CI. Nobody has run a real agent through it there yet. |
| macOS | From source | The test suite passes on macOS (Apple silicon) in CI. Nobody has run a real agent through it there yet. |

On Linux and macOS, the search of your path for agents has not been run on a real machine; CI deliberately skips it because a build server has no agents. The program-finding step recognizes ELF programs, Mach-O programs and `#!` scripts.

## Status and limits

- Early access, version 0.1.x. The npm package is Windows x64 only, and the binary is not code signed.
- Failover rests on recognizing usage-limit text. If a provider changes its wording, a switch can be missed or, rarely, a conversation that quotes a limit message can trigger one. The detector needs the limit wording together with a reset or retry hint and a quiet provider to reduce this.
- It is not an operating system sandbox.
- Adapters are trusted code, and a provider manifest names an executable that Uuldana will launch, so install only manifests you trust.

## Documentation

- [Architecture](docs/ARCHITECTURE.md) and the [decision records](docs/adr/README.md)
- [Configuration](docs/CONFIGURATION.md)
- [Provider SDK](docs/PROVIDER_SDK.md)
- [Plugin compatibility](docs/PLUGIN_COMPATIBILITY.md)
- [Protocol and platform notes](docs/PROTOCOL_COMPATIBILITY.md)
- [Contributing](CONTRIBUTING.md), [security policy](SECURITY.md), [releasing](docs/RELEASING.md)

## Contact

- Website and documentation: [uuldana-cli.pages.dev](https://uuldana-cli.pages.dev)
- Bugs and ideas: [GitHub Issues](https://github.com/Adjie-Dev/uuldana-cli/issues)
- Security problems, please privately: aniccacodestudio@gmail.com (see [SECURITY.md](SECURITY.md))
- Business questions: aniccacodestudio@gmail.com
- Instagram: [instagram.com/balindrakumara](https://www.instagram.com/balindrakumara)

## License

Licensed under the [MIT License](LICENSE-MIT). Copyright (c) 2026 Ang Hae Jhie (Anicca Code). The licenses of the libraries that are built into the binary are in [THIRD_PARTY_NOTICES.md](THIRD_PARTY_NOTICES.md).
