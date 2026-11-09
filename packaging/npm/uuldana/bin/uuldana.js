#!/usr/bin/env node
'use strict';

const { spawn } = require('node:child_process');
const path = require('node:path');

if (process.platform !== 'win32' || process.arch !== 'x64') {
  console.error(`uuldana: no build for ${process.platform}-${process.arch} yet. Supported: win32-x64.`);
  process.exit(1);
}

const binary = path.join(__dirname, 'uuldana.exe');

// The terminal belongs to the child. Ctrl+C and friends are handled there.
for (const signal of ['SIGINT', 'SIGTERM', 'SIGHUP']) process.on(signal, () => {});

const child = spawn(binary, process.argv.slice(2), { stdio: 'inherit', windowsHide: false });
child.on('error', (error) => {
  console.error(`uuldana: cannot start ${binary}: ${error.message}`);
  process.exit(1);
});
child.on('exit', (code, signal) => {
  if (signal) process.kill(process.pid, signal);
  else process.exit(code ?? 1);
});
