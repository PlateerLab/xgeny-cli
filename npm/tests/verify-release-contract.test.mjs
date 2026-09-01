import assert from 'node:assert/strict';
import test from 'node:test';

import { normalizeDryRunReport } from '../scripts/verify-release.mjs';

const report = {
  id: '@xgen/cli@0.1.0-rc.3',
  name: '@xgen/cli',
  version: '0.1.0-rc.3',
  files: [],
};

test('npm 10 and npm 11 dry-run report shapes normalize identically', () => {
  assert.equal(normalizeDryRunReport(report), report);
  assert.equal(normalizeDryRunReport({ '@xgen/cli': report }), report);
});

test('dry-run normalization rejects ambiguous package reports', () => {
  assert.throws(
    () => normalizeDryRunReport({ first: report, second: { ...report } }),
    /exactly one package/,
  );
  assert.throws(() => normalizeDryRunReport([]));
});
