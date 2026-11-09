# uuldana-cli

Runs the AI coding agents installed on your machine, each in its own terminal interface under one header, and moves the work to the next agent when one of them has used up its usage limit.

```
npm install -g uuldana-cli
uuldana
```

The package is called `uuldana-cli`. The command it installs is `uuldana`. You need Node.js 18 or newer; it only runs a small launcher, and the program itself is a native binary.

## What it does

- Starts the first available agent in your project directory, in the agent's own terminal interface.
- When the agent prints its usage limit, stops it gracefully and continues on the next agent in your order, handing it the visible conversation as its first prompt. No Ctrl+C, no retyping, no new sign-in.
- Keeps a journal of each session, so `uuldana resume` continues the last session of a directory.
- Keeps a permission ceiling you set. An agent that cannot enforce it is refused, and failover never raises it.

Each agent stays itself: its own sign-in, plugins, skills and MCP servers. Uuldana does not translate them between agents.

## Commands

| Command | What it does |
| --- | --- |
| `uuldana` | Start a session in this directory |
| `uuldana resume` | Continue the last session of this directory |
| `uuldana providers` | List the agents found and what was learned about them |
| `uuldana --version` | Print the version |
| `--bypass-permissions` | Never ask, no limits, for this run only |

Inside a session, type `/agent` and Enter on an empty line to choose the order of the agents.

## No setup, and what the first start does

The first start asks the programs on your search path for their `--help`, once, to find the agents. That means running them, with a reduced environment (no API keys or tokens passed), stdin closed, five seconds each, and a temporary folder of their own. It can take up to a minute, and the answer is kept for the next start. An optional `config.json` sets the order of the agents; with `"auto_detect": false` in it, only the agents you list are ever run.

## Status

Early access (0.1.x), Windows x64 only, and the binary is not code signed, so Windows SmartScreen may warn. Four agents were tried against real installs; others were not. Failover rests on recognizing usage-limit text, which is not a documented interface and may change.

On Linux and macOS, build from source: see the repository.

## Links

- Website and documentation: https://uuldana-cli.pages.dev
- Source and issues: https://github.com/Adjie-Dev/uuldana-cli
- Security problems, privately: aniccacodestudio@gmail.com
- Instagram: [instagram.com/balindrakumara](https://www.instagram.com/balindrakumara)

MIT License, copyright (c) 2026 Ang Hae Jhie (Anicca Code). The libraries built into the binary are listed in THIRD_PARTY_NOTICES.md.
