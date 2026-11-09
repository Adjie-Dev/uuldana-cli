# Uuldana

Uuldana runs the coding agents you already use, in their own terminal interfaces, and keeps your work going when one of them runs out of quota.

It is a supervisor, not another agent. Each agent stays itself. Its tools, plugins, skills, MCP servers and sign-ins are the ones you have already set up. Uuldana only starts them, watches them, remembers what happened and, when the active one hits a usage limit, continues the work on the next one in the order you chose.

```
uuldana              start the first available agent in this directory
uuldana resume       continue the last session of this directory
uuldana providers    list the agents found on this machine and what was learned about them
uuldana --version

uuldana --bypass-permissions    start (or resume) without approval prompts or limits, this run only
```

There is nothing to set up, and no agent is known by name. Uuldana looks at the console programs installed on your machine, asks each one for its `--help` once, and takes the ones whose help describes a coding agent: a way to start with a prompt, a way to resume earlier work, and permission or model options. It learns how to start, resume and set permissions from the flags that help lists. A new agent you install is found the next time you start Uuldana. Asking a program for its help means running it, with a reduced environment (no API keys or tokens); [SECURITY.md](SECURITY.md) says what that implies, and `"auto_detect": false` in `config.json` limits Uuldana to the agents you list. `config.json` is only for choosing the order, adding accounts or setting a permission ceiling.

Inside a session, type `/agent` and Enter on an empty line to choose which agent works first and which ones take over after it. See [choosing the order inside a session](docs/CONFIGURATION.md#choosing-the-order-inside-a-session).

## What it does

- **Native terminal inside Uuldana's frame.** The provider's own UI runs in a pseudo terminal (ConPTY on Windows, a Unix PTY elsewhere) and your keyboard goes to it unchanged. Its screen is drawn under a header that always shows Uuldana, which agent is working and any warning (such as permissions bypassed), so an agent that clears the screen or repaints cannot remove it. Uuldana uses the terminal's alternate screen, like a full screen program, and gives your terminal back on exit. Resizing follows your terminal. The mouse wheel scrolls the agent's earlier output on Uuldana's screen; Shift+PageUp and Shift+PageDown do the same, and any other key returns to the live screen. An agent that asks for the mouse itself (Codex does) is sent the wheel and scrolls its own screen, as it would in a plain terminal. Because Uuldana takes the mouse, selecting text needs Shift held down while you drag.
- **Found by itself, ordered by you.** Without a configuration file Uuldana uses the agents it finds installed, in the order of their ids, and none is preferred over another. An agent's id is the name of its program without the extension. `config.json` lists agents in the order you want them used, and installed agents it does not list follow after them (unless `auto_detect` is `false`). The same agent can appear several times, each with its own environment, which is how a second account is selected (for example a different configuration directory variable the agent reads). Uuldana never reads or copies the credentials behind it.
- **Automatic failover, for any agent.** One general reader looks at what every agent prints for a message saying an allowance is used up, together with when it returns or what to do (resets, try again, upgrade, credits); a pause for a rate limit per minute is not a stop. This is a heuristic on text, so an agent that keeps working after such text is left running. When a provider prints its usage-limit message, Uuldana stops it gracefully, rests that entry for a while, hands the visible conversation to the next entry as its first prompt, and keeps going. No Ctrl+C, no retyping, no new sign-in.
- **Unified history.** Every run and event of a session is written to an append-only journal, and when a run ends, the text its screen showed (the last 120 lines with text) is written there too. `uuldana resume` rebuilds the session from it, even after a crash. An agent whose help offers a flag to choose the session id is started with an id Uuldana knows and resumed by it; any other agent is resumed with its own flag for "continue the latest conversation of this folder", when it has one.
- **Permissions that do not creep.** You set a ceiling for approval, filesystem and network. A provider that cannot enforce the ceiling with its own flags is refused instead of being launched with more permission. Failover never raises it.

## What it does not do

- It does not translate plugins. A plugin works whenever the provider that owns it is the one running. See [plugin compatibility](docs/PLUGIN_COMPATIBILITY.md).
- It does not move hidden reasoning, memory, approvals or running tool calls between providers. A handoff carries the visible conversation and says so. Uuldana reads no agent's own session files; it carries the plain text the agent's screen showed (the last 120 lines with text, interface text included), so the next agent still knows what was asked. When you move the work yourself with `/agent`, the conversation is pasted into the new agent's input box and not sent, so nothing is spent until you press Enter. When a usage limit moves it, the new agent starts on it at once, because you are not there to press Enter. If nobody has spoken to the agent yet there is nothing to carry, and the next agent starts clean.
- It is not an operating system sandbox. Permission mapping describes what each provider's flags enforce.

## Install

There are no published packages yet. Build from source with a current stable Rust toolchain (edition 2024):

```
cargo build --release -p uuldana-cli
```

The binary is `target/release/uuldana`. On Windows the GNU toolchain needs a complete MinGW-w64 install and the MSVC toolchain needs the Visual Studio Build Tools; see [Windows development](docs/WINDOWS_DEVELOPMENT.md). `scripts/package-release.ps1` and `scripts/package-release.sh` build an archive with checksums.

## Configure

Create the configuration file (its location is printed when it is missing):

| Platform | Default location |
| --- | --- |
| Windows | `%APPDATA%\uuldana\config.json` |
| Linux, macOS | `$XDG_CONFIG_HOME/uuldana/config.json`, or `~/.config/uuldana/config.json` |

`UULDANA_CONFIG` overrides it. A minimal file:

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

An id is the name of the agent's program without the extension, as `uuldana providers` lists it. An entry that matches no agent found on the machine is left out, with a notice. See [configuration](docs/CONFIGURATION.md) for every field, including accounts, cooldowns and provider manifests.

## How agents are found

Uuldana keeps no list of agents. At start it reads the search path, leaving out the operating system's own folders, and takes the console programs it finds (on Windows, programs whose header says console and not window, and the programs behind npm script shims). Each one is asked for `--help` once, with stdin closed and five seconds to answer. A program whose help describes a coding agent becomes one:

- a way to start with a first prompt (an option for it, or a positional argument);
- a way to resume earlier work (a flag with a session id, a subcommand, or a flag that continues the latest conversation of the folder);
- permission controls (a skip-everything flag, a mode flag with named choices, or sandbox and approval flags with named choices) or a model option.

Permission modes are mapped from the words of the choices (a plan or read-only mode asks about everything and writes nothing; an accept-edits mode asks about risky actions; a bypass mode never asks). A program run without any permission flag is assumed to ask before acting, and `uuldana providers` marks that as an assumption. What the help does not say is left unknown, not guessed.

The answers are kept in `programs.json` in the data directory, keyed by the program's path, size and modification time, so a program is asked again only after it changes. The first look at a big search path took between fifteen and thirty seconds on the test machine and says so; after that the start is immediate (a tenth of a second for the lookup). `uuldana providers` lists what was found and why.

If something was learned wrongly, a [manifest](docs/PROVIDER_SDK.md#provider-manifests) with the same id in the `adapters` directory next to `config.json` overrides it.

Agents tried here on real installs: four, found by this method with no setup (each opened through Uuldana and left with Ctrl+C, and stopped gracefully the way a failover stops it). Behavior that needs a real exhausted quota (a live failover between two agents) has not been exercised for every agent, because that would burn an account; it is covered with real child processes standing in for agents, and with real usage-limit messages as test inputs. Agents that are not installed on the test machine have not been tried at all. Usage-limit messages are not a documented interface and may change.

## Platforms

Windows 11 (GNU and MSVC toolchains) is where Uuldana is developed and tested. Linux and macOS compile (`cargo check --workspace --all-targets` passes for `x86_64-unknown-linux-gnu` and `aarch64-apple-darwin`) and the CI workflow runs the tests there, but no Linux or macOS machine has run them yet.

## Documentation

- [Architecture](docs/ARCHITECTURE.md) and the [decision records](docs/adr/README.md)
- [Configuration](docs/CONFIGURATION.md)
- [Provider SDK](docs/PROVIDER_SDK.md)
- [Plugin compatibility](docs/PLUGIN_COMPATIBILITY.md)
- [Protocol and platform notes](docs/PROTOCOL_COMPATIBILITY.md)
- [Contributing](CONTRIBUTING.md), [security policy](SECURITY.md), [releasing](docs/RELEASING.md)

## Contact

Instagram: [instagram.com/balindrakumara](https://www.instagram.com/balindrakumara)

## License

Licensed under the [MIT License](LICENSE-MIT). Copyright (c) 2026 Ang Hae Jhie (Anicca Code). The licenses of the libraries that are built into the binary are in [THIRD_PARTY_NOTICES.md](THIRD_PARTY_NOTICES.md).
