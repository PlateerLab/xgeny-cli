import assert from 'node:assert/strict';
import test from 'node:test';

import { npmInvocation } from '../scripts/config.mjs';

test('Windows npm invocation bypasses the cmd shim without enabling a shell', () => {
  assert.deepEqual(
    npmInvocation(['pack', 'fixture'], {
      platform: 'win32',
      nodeExecutable: 'C:\\node\\node.exe',
      npmExecPath: null,
    }),
    {
      command: 'C:\\node\\node.exe',
      args: [
        'C:\\node\\node_modules\\npm\\bin\\npm-cli.js',
        'pack',
        'fixture',
      ],
    },
  );
});

test('npm_execpath pins the exact JavaScript CLI on every platform', () => {
  assert.deepEqual(
    npmInvocation(['view'], {
      platform: 'win32',
      nodeExecutable: 'C:\\node\\node.exe',
      npmExecPath: 'D:\\npm\\npm-cli.js',
    }),
    {
      command: 'C:\\node\\node.exe',
      args: ['D:\\npm\\npm-cli.js', 'view'],
    },
  );
});

test('Unix npm invocation retains the PATH executable fallback', () => {
  assert.deepEqual(
    npmInvocation(['root'], {
      platform: 'linux',
      nodeExecutable: '/opt/node/bin/node',
      npmExecPath: null,
    }),
    { command: 'npm', args: ['root'] },
  );
});
