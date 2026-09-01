import assert from 'node:assert/strict';
import test from 'node:test';

import {
  publicationOrder,
  validateExistingPublication,
} from '../scripts/publish.mjs';

const provenance = {
  provenance: { predicateType: 'https://slsa.dev/provenance/v1' },
};

test('platform packages are ordered before the launcher', () => {
  const reports = [
    { name: '@xgen/cli', integrity: 'sha512-launcher' },
    { name: '@xgen/cli-linux-x64-musl', integrity: 'sha512-linux' },
    { name: '@xgen/cli-darwin-arm64', integrity: 'sha512-macos' },
  ];
  assert.deepEqual(
    publicationOrder(reports, '@xgen/cli').map(({ name }) => name),
    ['@xgen/cli-linux-x64-musl', '@xgen/cli-darwin-arm64', '@xgen/cli'],
  );
});

test('publication order requires exactly one launcher', () => {
  assert.throws(
    () => publicationOrder([{ name: '@xgen/cli-linux-x64-musl' }], '@xgen/cli'),
    /exactly one launcher/,
  );
});

test('an identical retry requires integrity, dist-tag, and provenance', () => {
  const report = { name: '@xgen/cli', integrity: 'sha512-exact' };
  const remote = {
    'dist.integrity': 'sha512-exact',
    'dist-tags': { next: '0.1.0-rc.3' },
    'dist.attestations': provenance,
  };
  assert.doesNotThrow(() =>
    validateExistingPublication(remote, report, '0.1.0-rc.3', 'next'),
  );
  assert.throws(
    () =>
      validateExistingPublication(
        { ...remote, 'dist.integrity': 'sha512-different' },
        report,
        '0.1.0-rc.3',
        'next',
      ),
    /already differs/,
  );
  assert.throws(
    () =>
      validateExistingPublication(
        { ...remote, 'dist.attestations': undefined },
        report,
        '0.1.0-rc.3',
        'next',
      ),
    /missing npm provenance/,
  );
});
