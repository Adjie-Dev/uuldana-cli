# uuldana-cli

Runs the coding agents installed on your machine (Claude Code, Codex, and others) in their own terminal UI under one header, and moves the work to the next agent when one of them has used up its usage limit.

```
npm install -g uuldana-cli
uuldana
```

The package is called `uuldana-cli`; the command it installs is `uuldana`.

No setup is needed. The first start asks the programs on your search path for their `--help`, once, to find the agents. That means running them, with a reduced environment (no API keys or tokens passed). It can take up to a minute, and the answer is kept for the next start. `uuldana providers` lists what was found. An optional `config.json` sets the order of the agents; with `"auto_detect": false` in it, only the agents you list are ever run.

Early access (0.1.x), Windows x64 only, and the binary is not code signed, so Windows SmartScreen may warn. Claude Code and Codex were tried against real installs; other agents were not.

Bugs, ideas, or business questions: aniccacodestudio@gmail.com

Instagram: [instagram.com/balindrakumara](https://www.instagram.com/balindrakumara)

MIT License, copyright (c) 2026 Ang Hae Jhie (Anicca Code). The libraries built into the binary are listed in THIRD_PARTY_NOTICES.md.
