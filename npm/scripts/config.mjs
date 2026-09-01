import assert from 'node:assert/strict';
import { readFile } from 'node:fs/promises';
import path from 'node:path';
import { fileURLToPath } from 'node:url';

export const npmRoot = path.resolve(path.dirname(fileURLToPath(import.meta.url)), '..');
export const repoRoot = path.resolve(npmRoot, '..');
export const launcherRoot = path.join(npmRoot, 'packages', 'cli');
export const repositoryUrl = 'git+https://github.com/PlateerLab/xgeny-cli.git';
export const nodeEngine = '>=22.14.0';
export const npmPackageManager = 'npm@11.19.0';

export async function readJson(filename) {
  return JSON.parse(await readFile(filename, 'utf8'));
}

export async function cargoVersion() {
  const manifest = await readFile(path.join(repoRoot, 'Cargo.toml'), 'utf8');
  const section = manifest.match(/\[workspace\.package\]([\s\S]*?)(?:\n\[|$)/);
  assert(section, 'Cargo.toml workspace.package section is missing');
  const version = section[1].match(/^version\s*=\s*"([^"]+)"\s*$/m);
  assert(version, 'Cargo.toml workspace package version is missing');
  return version[1];
}

export async function distributionMetadata() {
  const [catalog, launcherCatalog, launcher, workspace, lock, version] = await Promise.all([
    readJson(path.join(npmRoot, 'platforms.json')),
    readJson(path.join(launcherRoot, 'platforms.json')),
    readJson(path.join(launcherRoot, 'package.json')),
    readJson(path.join(npmRoot, 'package.json')),
    readJson(path.join(npmRoot, 'package-lock.json')),
    cargoVersion(),
  ]);
  return { catalog, launcherCatalog, launcher, workspace, lock, version };
}

export function platformPackageJson(specification, version) {
  return {
    name: specification.packageName,
    version,
    description: `Native XGENy CLI binary for ${specification.target}`,
    license: 'Apache-2.0',
    repository: { type: 'git', url: repositoryUrl },
    homepage: 'https://github.com/PlateerLab/xgeny-cli#readme',
    bugs: { url: 'https://github.com/PlateerLab/xgeny-cli/issues' },
    engines: { node: nodeEngine },
    os: [specification.os],
    cpu: [specification.cpu],
    files: [
      specification.binaryPath,
      'README.md',
      'LICENSE',
      'NATIVE_RUNTIME_PROVENANCE.md',
      'THIRD_PARTY_LICENSES.txt',
    ],
    publishConfig: {
      access: 'public',
      provenance: true,
      registry: 'https://registry.npmjs.org/',
    },
  };
}

export function npmCommand() {
  return process.platform === 'win32' ? 'npm.cmd' : 'npm';
}
