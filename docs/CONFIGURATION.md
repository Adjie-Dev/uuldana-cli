# Configuration

Uuldana works without any configuration: it uses the agents it finds installed. The file is for choosing the order, adding accounts and setting a permission ceiling. Nothing in it is a default priority: the order of `providers` is the order Uuldana tries them, and installed agents it does not list come after them.

## Location

| Platform | Default |
| --- | --- |
| Windows | `%APPDATA%\uuldana\config.json` |
| Linux, macOS | `$XDG_CONFIG_HOME/uuldana/config.json`, otherwise `~/.config/uuldana/config.json` |

Set `UULDANA_CONFIG` to use another file. Provider manifests live in an `adapters` directory next to the configuration file.

Session journals and the cooldown table are stored under the data directory: `UULDANA_DATA`, otherwise `%LOCALAPPDATA%\uuldana` on Windows and `$XDG_DATA_HOME/uuldana` (or `~/.local/share/uuldana`) elsewhere. Journals are in `sessions/`.

## Fields

```json
{
  "version": 1,
  "permissions": {
    "approval": "ask_for_risky",
    "filesystem": "workspace_write",
    "network": "allowed"
  },
  "cooldown_minutes": 300,
  "failover": true,
  "providers": [
    { "id": "claude-code", "label": "claude-work",
      "env": { "CLAUDE_CONFIG_DIR": "D:\\accounts\\work" } },
    { "id": "claude-code", "label": "claude-personal" },
    { "id": "codex", "cooldown_minutes": 120 }
  ]
}
```

| Field | Meaning |
| --- | --- |
| `version` | Must be `1`. |
| `permissions` | The ceiling for every session. When it is absent altogether, the ceiling is `ask_for_risky`, `unrestricted`, `allowed`: the agents ask before risky actions and Uuldana adds no restriction of its own. When it is present, the fields it leaves out mean the strictest value. |
| `permissions.approval` | `ask_every_action`, `ask_for_risky` or `never_ask`. |
| `permissions.filesystem` | `read_only`, `workspace_write` or `unrestricted`. |
| `permissions.network` | `denied` or `allowed`. |
| `cooldown_minutes` | How long an entry rests after a usage limit when the provider does not say when it resets. Default 300. |
| `failover` | `false` keeps a session on its first provider. Default `true`. Failover also needs at least two entries. |
| `auto_detect` | `false` uses only the agents listed in `providers`, and no other program on the search path is run to find out what it is. Default `true`: installed agents that are not listed follow after the listed ones, so an agent you install later is found without editing anything. |
| `providers` | Entries in priority order. Optional unless `auto_detect` is `false`. |
| `providers[].id` | The id of an agent found on this machine, which is the name of its program without the extension, or the id of a manifest (see `uuldana providers`). An entry that matches nothing is left out, with a notice. |
| `providers[].label` | A unique name for the entry. Defaults to the id; give each account of the same agent its own label. |
| `providers[].env` | Extra environment variables for that provider's process only. Used to select an account. |
| `providers[].cooldown_minutes` | Per-entry override of the rest period. |

## Without a configuration file

With no file, or a file with no `providers`, Uuldana starts the installed agents in the order of their ids, none preferred over another. The permission ceiling is `ask_for_risky`, `unrestricted`, `allowed`, as when `permissions` is absent. Every agent that was learned can start under it, because each one has at least its own default way of running, which is assumed to ask before acting. The strictest ceiling (`"network": "denied"` included) starts no agent, because a program's flags do not say it can block network access. Nothing is written until you choose an order with `/agent`; then the file is created with the agents you picked, and the ones you did not pick keep being found by themselves.

## Permissions

The ceiling is what you allow, not what the agent promises. For each agent, Uuldana maps the ceiling to the flags its help lists (a skip-everything flag, a mode flag with named choices, sandbox and approval flags) and reports honestly what it cannot. When no mode fits inside the ceiling, that entry is not started and Uuldana says so. For example, no agent's help lists a flag that blocks network access, so a ceiling with `"network": "denied"` starts none. Lower the ceiling's strictness or use an agent that can enforce it.

`uuldana --bypass-permissions` (also `uuldana resume --bypass-permissions`) replaces the configured ceiling with "never ask, unrestricted filesystem, network allowed" for that invocation only; the file is not changed. Each provider then gets its own most permissive flags, for example `--dangerously-skip-permissions` for Claude Code. A session's ceiling is fixed when it is created, so `resume --bypass-permissions` is refused for a session that was created with a stricter ceiling; start a new session instead.

Failover reuses the same ceiling for every run of a session. A configuration that becomes stricter after a session began applies to the next run of that session.

## Accounts

An `env` entry changes the environment of one provider process. Which variable selects an account is the provider's business: Claude Code uses `CLAUDE_CONFIG_DIR`, Codex uses `CODEX_HOME`. Uuldana reads nothing stored there, neither credentials nor conversation files. To continue a conversation on another provider it uses the plain text of the previous agent's screen (see Failover behavior below).

## Failover behavior

1. The first entry that is not resting and can be started runs.
2. If the provider prints its usage-limit message and stays quiet for 1.5 seconds, it is stopped gracefully and the entry rests for `cooldown_minutes`. One general reader reads every native terminal agent. It does not keep a list of sentences: it reads a word that says an allowance is gone (hit, reached, exceeded, exhausted, out of) next to a word that names it (limit, quota, credits), or an error code, and scores what stands around it: when the allowance returns, a time, being addressed, a terse line. Weekly, 5-hour, per-session and per-model limits are the same to it. Only an allowance that is gone moves the work. A warning that it is only partly used (a share below 100%, "approaching", "close to", "10 left"), a limit per minute, a question, source code and a quoted string are never taken for one. The text that stopped an agent is taken out of what the next agent is given, so it cannot show it back and look as if it had run out too. A manifest may list `limit_phrases` of its own for wording the reader does not know. Text that looked like a stop and was turned down is written to the journal as a diagnostic with the reason. If the agent keeps working for 20 seconds after text that looked like a limit, the text is taken for something else, the agent is left running, a notice says so and the journal records it. If a provider ever reports an absolute reset time, that time is used instead; the general reader does not report one today.
3. The next entry that is available starts with the visible conversation as its first prompt. If nobody has spoken to the agent that stopped, there is nothing to carry and the next one starts without a prompt. When you move the work yourself with `/agent` instead, the conversation is pasted into the new agent's input box and not sent: nothing runs until you press Enter. Uuldana reads no agent's own history; the prompt carries the plain text of the previous agent's screen, the last 120 lines with text. That text is also written to the journal when the run ends, so it is still there for `resume` and for a later handoff.
4. If every entry is resting, Uuldana says when the first one returns.

Resting entries are remembered across runs in the cooldown table, so a new `uuldana` does not walk back into an exhausted account. A provider that quits on its own with a failing exit status after a limit message counts the same way; a normal exit after the message is treated as you leaving.

## Choosing the order inside a session

Besides editing `providers`, you can set the order while a session runs. Type `/agent` as a whole line and press Enter. Uuldana keeps that line from the agent (it erases what was typed before the agent sees an Enter) and opens a menu in the agent's area of Uuldana's frame:

1. Pick the agent that works first. Up and Down, a number, or Enter choose; Esc leaves without changing anything.
2. Pick the one that takes over when the first reaches its usage limit, then the next, and so on. The row "No more" ends the picking and keeps every agent you did not pick in its current order, behind the ones you picked.

Agents installed on the machine that are not in `config.json` are listed too, marked as new. Choosing one adds `{ "id": "..." }` to `providers`.

What happens afterwards:

- The order is written back to `config.json`, so the next `uuldana` starts with it. Only the order of `providers` (and added entries) changes. The file is rewritten as formatted JSON with sorted keys, so hand formatting is not kept.
- If the agent you picked first is not the one running, the work moves to it now: the running agent is stopped gracefully and the new one starts with the visible conversation as its first prompt, exactly as in a failover. This also works with `"failover": false`.
- If you pick an agent that is resting after a usage limit as the first one, its rest ends: choosing it says its quota is back.
- If the running agent stays first, it keeps running and its screen comes back as it was; only the agents behind it change.

The command is recognized only when the whole line is `/agent` (trailing spaces allowed). A line that has been edited with the cursor keys, completed with Tab, or pasted with control sequences is passed to the agent untouched, because the line cannot be followed reliably. If the configuration file cannot be written, the new order applies to the current session only and a message says so.

## Provider manifests

See [Provider SDK, provider manifests](PROVIDER_SDK.md#provider-manifests).

A manifest in your `adapters` directory adds an agent the program does not need to be asked about, or overrides what was learned for the same id. With no `limit_phrases` it is still covered by the general limit reader.
