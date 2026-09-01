#!/usr/bin/env node
'use strict';

const { spawnSync } = require('node:child_process');
const catalog = require('../platforms.json');
const { resolvePlatformBinary, selectPlatform } = require('../lib/platform.cjs');

let specification;
let binary;
try {
  specification = selectPlatform(catalog, process.platform, process.arch);
  binary = resolvePlatformBinary(specification);
} catch (error) {
  console.error(`xgeny: ${error.message}`);
  process.exitCode = 1;
  return;
}

const result = spawnSync(binary, process.argv.slice(2), {
  env: process.env,
  stdio: 'inherit',
  windowsHide: true,
});

if (result.error) {
  const code = typeof result.error.code === 'string' ? ` (${result.error.code})` : '';
  console.error(`xgeny: failed to start the native binary${code}`);
  process.exitCode = 1;
} else if (result.signal) {
  console.error(`xgeny: native process terminated by ${result.signal}`);
  process.exitCode = 1;
} else {
  process.exitCode = result.status ?? 1;
}
