import assert from 'node:assert/strict';
import { lstat, readFile } from 'node:fs/promises';
import path from 'node:path';

import {
  distributionMetadata,
  launcherRoot,
  nodeEngine,
  npmPackageManager,
  npmRoot,
  platformPackageJson,
  repositoryUrl,
} from './config.mjs';

const { catalog, launcherCatalog, launcher, workspace, lock, version } =
  await distributionMetadata();

assert.equal(catalog.schemaVersion, 1);
assert.deepEqual(launcherCatalog, catalog, 'launcher platform catalog must be byte-equivalent JSON');
assert.equal(catalog.platforms.length, 5);
assert.equal(workspace.private, true);
assert.equal(workspace.version, version);
assert.equal(workspace.packageManager, npmPackageManager);
assert.deepEqual(workspace.engines, { node: nodeEngine });
assert.deepEqual(Object.keys(workspace.scripts).sort(), ['check', 'test']);
assert.equal(lock.name, workspace.name);
assert.equal(lock.version, version);
assert.equal(lock.lockfileVersion, 3);
assert.equal(lock.packages[''].name, workspace.name);
assert.equal(lock.packages[''].version, version);
assert.deepEqual(lock.packages[''].engines, workspace.engines);

assert.equal(launcher.name, '@xgen/cli');
assert.equal(launcher.version, version);
assert.equal(launcher.license, 'Apache-2.0');
assert.deepEqual(launcher.repository, { type: 'git', url: repositoryUrl });
assert.deepEqual(launcher.engines, { node: nodeEngine });
assert.deepEqual(launcher.bin, { xgeny: 'bin/xgeny.cjs' });
assert.equal(launcher.scripts, undefined, 'published launcher must not have lifecycle scripts');
assert.deepEqual(launcher.publishConfig, {
  access: 'public',
  provenance: true,
  registry: 'https://registry.npmjs.org/',
});
assert.deepEqual(launcher.files, [
  'bin/xgeny.cjs',
  'lib/platform.cjs',
  'platforms.json',
  'README.md',
  'LICENSE',
]);

const unique = (values, label) =>
  assert.equal(new Set(values).size, values.length, `${label} must be unique`);
unique(catalog.platforms.map(({ target }) => target), 'targets');
unique(catalog.platforms.map(({ packageName }) => packageName), 'package names');
unique(catalog.platforms.map(({ asset }) => asset), 'release assets');
unique(catalog.platforms.map(({ tarball }) => tarball), 'npm tarballs');
unique(catalog.platforms.map(({ os, cpu }) => `${os}/${cpu}`), 'runtime platforms');

const expectedOptionalDependencies = {};
for (const specification of catalog.platforms) {
  assert.match(specification.target, /^[A-Za-z0-9_-]+$/);
  assert.match(specification.packageName, /^@xgen\/cli-[a-z0-9-]+$/);
  assert.match(specification.asset, /^xgeny-[A-Za-z0-9_.-]+$/);
  assert.match(specification.binaryPath, /^bin\/xgeny(?:\.exe)?$/);
  assert.match(specification.tarball, /^xgen-cli-[a-z0-9-]+\.tgz$/);
  assert.ok(['linux', 'darwin', 'win32'].includes(specification.os));
  assert.ok(['x64', 'arm64'].includes(specification.cpu));
  if (specification.os === 'win32') {
    assert.equal(specification.binaryPath, 'bin/xgeny.exe');
  } else {
    assert.equal(specification.binaryPath, 'bin/xgeny');
  }
  expectedOptionalDependencies[specification.packageName] = version;
  const generated = platformPackageJson(specification, version);
  assert.equal(generated.scripts, undefined);
  assert.equal(generated.libc, undefined, 'static musl packages must remain installable on glibc');
  assert.deepEqual(generated.os, [specification.os]);
  assert.deepEqual(generated.cpu, [specification.cpu]);
  assert.deepEqual(generated.repository, { type: 'git', url: repositoryUrl });
}
assert.deepEqual(launcher.optionalDependencies, expectedOptionalDependencies);

for (const relative of [
  'package.json',
  'README.md',
  'platforms.json',
  'bin/xgeny.cjs',
  'lib/platform.cjs',
]) {
  const entry = await lstat(path.join(launcherRoot, relative));
  assert.equal(entry.isFile(), true, `${relative} must be a regular file`);
  assert.equal(entry.isSymbolicLink(), false, `${relative} must not be a symbolic link`);
}

for (const relative of ['Cargo.toml', 'LICENSE', 'THIRD_PARTY_LICENSES.txt']) {
  const entry = await lstat(path.join(npmRoot, '..', relative));
  assert.equal(entry.isFile(), true, `${relative} must be a regular file`);
  assert.equal(entry.isSymbolicLink(), false, `${relative} must not be a symbolic link`);
}

const launcherSource = await readFile(path.join(launcherRoot, 'bin', 'xgeny.cjs'), 'utf8');
assert.ok(launcherSource.startsWith('#!/usr/bin/env node\n'));
assert.equal(/\b(?:fetch|https?\.request|https?\.get)\b/.test(launcherSource), false);
assert.equal(/postinstall|preinstall/.test(JSON.stringify(launcher)), false);

console.log(`npm distribution contract: PASS (${launcher.name} ${version}, 5 platforms)`);
