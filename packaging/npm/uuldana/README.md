# uuldana

Runs the coding agents installed on your machine (Claude Code, Codex, and others) in their own terminal UI under one header, and moves the work to the next agent when one of them has used up its usage limit.

```
npm install -g uuldana
uuldana
```

No setup is needed. The first start asks the programs on your search path for their `--help`, once, to find the agents. That can take up to a minute, and the answer is kept for the next start. `uuldana providers` lists what was found. An optional `config.json` sets the order of the agents.

Early version, Windows x64 only. Claude Code and Codex were tried against real installs; other agents were not. This package only launches the native binary from the platform package for your machine.

Bugs, ideas, or business questions: aniccacodestudio@gmail.com

MIT License, copyright (c) 2026 Ang Hae Jhie (Anicca Code). The libraries built into the binary are listed in THIRD_PARTY_NOTICES.md.
