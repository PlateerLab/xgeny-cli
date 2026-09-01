import assert from 'node:assert/strict';
import { createRequire } from 'node:module';
import { readFile } from 'node:fs/promises';
import test from 'node:test';

const require = createRequire(import.meta.url);
const catalog = JSON.parse(
  await readFile(new URL('../platforms.json', import.meta.url), 'utf8'),
);
const { resolvePlatformBinary, selectPlatform } = require('../packages/cli/lib/platform.cjs');

const expected = [
  ['linux', 'x64', '@xgen/cli-linux-x64-musl', 'bin/xgeny'],
  ['linux', 'arm64', '@xgen/cli-linux-arm64-musl', 'bin/xgeny'],
  ['darwin', 'x64', '@xgen/cli-darwin-x64', 'bin/xgeny'],
  ['darwin', 'arm64', '@xgen/cli-darwin-arm64', 'bin/xgeny'],
  ['win32', 'x64', '@xgen/cli-win32-x64', 'bin/xgeny.exe'],
];

test('platform catalog selects every supported native package exactly', () => {
  for (const [platform, architecture, packageName, binaryPath] of expected) {
    const selected = selectPlatform(catalog, platform, architecture);
    assert.equal(selected.packageName, packageName);
    assert.equal(selected.binaryPath, binaryPath);
  }
});

test('unsupported platform fails without a fallback download', () => {
  assert.throws(
    () => selectPlatform(catalog, 'freebsd', 'x64'),
    (error) =>
      error.code === 'XGENY_UNSUPPORTED_PLATFORM' &&
      error.message === 'XGENy does not provide a native binary for freebsd/x64',
  );
});

test('native package resolution uses the exact package subpath', () => {
  const selected = selectPlatform(catalog, 'linux', 'x64');
  const requests = [];
  const resolved = resolvePlatformBinary(selected, (request) => {
    requests.push(request);
    return '/verified/native/xgeny';
  });
  assert.equal(resolved, '/verified/native/xgeny');
  assert.deepEqual(requests, ['@xgen/cli-linux-x64-musl/bin/xgeny']);
});

test('missing optional dependency produces a bounded recovery message', () => {
  const selected = selectPlatform(catalog, 'linux', 'x64');
  assert.throws(
    () =>
      resolvePlatformBinary(selected, () => {
        const error = new Error('host-specific resolution detail');
        error.code = 'MODULE_NOT_FOUND';
        throw error;
      }),
    (error) =>
      error.code === 'XGENY_PLATFORM_PACKAGE_MISSING' &&
      error.message ===
        'The required XGENy native package @xgen/cli-linux-x64-musl is not installed. ' +
          'Reinstall @xgen/cli without --omit=optional.' &&
      !error.message.includes('host-specific'),
  );
});
