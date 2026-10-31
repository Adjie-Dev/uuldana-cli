# Plugin compatibility

Uuldana does not run plugins, skills, MCP servers, hooks or custom commands. Each provider does, from its own configuration. Uuldana's contribution is to leave all of that alone and to keep it working when the provider changes.

## What continues

- **Every native extension keeps working in its own provider.** Uuldana starts the provider's real executable with its normal arguments and environment. Plugins, skills, MCP servers, instruction files such as `CLAUDE.md` or `AGENTS.md`, hooks and slash commands load exactly as they do when you start the provider yourself. Adapters never pass flags that disable extension discovery (such as a "bare" or "safe" mode) unless you asked for that.
- **Accounts keep their own setup.** A second account, selected with an `env` entry such as `CLAUDE_CONFIG_DIR`, has its own plugins and settings directory. Switching accounts of the same provider therefore also switches that account's plugins.
- **Project files continue.** The work itself lives in your workspace. When work moves to another provider, the next agent starts in the same directory, with the same files, the same git state and the same project instruction files that it supports.

## What does not move

When a run moves from one provider to another, only the visible conversation is carried, as the first prompt of the next provider. These are not carried, and the prompt says so:

- reasoning the provider did not display;
- the provider's memory and learned preferences;
- approvals granted inside the provider (the next one asks again);
- tool calls that were still running;
- any other internal state.

## What Uuldana does not promise

Providers do not share a plugin format. A Claude Code plugin does nothing in Codex, a Codex skill does nothing in Gemini CLI, and an MCP server configured for one provider is not configured for another unless you configured it there too. Uuldana does not translate, copy or synchronize extensions between providers, and it does not promise that two providers behave the same with the same project files.

Two practical consequences:

1. If you rely on an extension, put the providers that support it first in `providers`, or install the equivalent in each provider you list.
2. Project instruction files are the portable part. Providers that read the same file name (for example `AGENTS.md`) see the same instructions after a handoff. Providers that read a different file do not, and the handoff prompt is the only context they get.

## Checking what a provider offers

`uuldana providers` shows which providers are installed. Each adapter declares its capabilities (native terminal, initial prompt, resume by id, permission controls) as supported, unsupported or unknown, with the evidence behind it. Unknown never counts as supported: if an adapter cannot prove that a provider takes an initial prompt, a handoff to it is refused instead of guessed.
