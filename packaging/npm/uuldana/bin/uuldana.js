#!/usr/bin/env node
'use strict';

const { spawn } = require('node:child_process');
const path = require('node:path');

const PACKAGES = {
  'win32-x64': { name: '@uuldana/win32-x64', exe: 'uuldana.exe' },
};

const key = `${process.platform}-${process.arch}`;
const target = PACKAGES[key];
if (!target) {
  console.error(`uuldana: no build for ${key} yet. Supported: ${Object.keys(PACKAGES).join(', ')}.`);
  process.exit(1);
}

let binary;
try {
  const manifest = require.resolve(`${target.name}/package.json`);
  binary = path.join(path.dirname(manifest), 'bin', target.exe);
} catch {
  console.error(`uuldana: the package ${target.name} is missing. Reinstall with: npm install -g uuldana`);
  process.exit(1);
}

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
