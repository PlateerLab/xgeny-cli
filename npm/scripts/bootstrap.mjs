import assert from 'node:assert/strict';
import { spawnSync } from 'node:child_process';
import {
  copyFile,
  lstat,
  mkdir,
  mkdtemp,
  rename,
  rm,
  writeFile,
} from 'node:fs/promises';
import os from 'node:os';
import path from 'node:path';

import {
  distributionMetadata,
  nodeEngine,
  npmInvocation,
  repoRoot,
  repositoryUrl,
} from './config.mjs';

const bootstrapVersion = '0.0.0-bootstrap.0';

function parseArguments(argv) {
  assert.deepEqual(argv.slice(0, 1), ['--output-dir']);
  assert.equal(argv.length, 2, 'usage: bootstrap.mjs --output-dir DIRECTORY');
  return path.resolve(argv[1]);
}

function manifest(name, specification) {
  return {
    name,
    version: bootstrapVersion,
    description: 'Non-executable package-name bootstrap for the XGENy npm trusted publisher',
    license: 'Apache-2.0',
    repository: { type: 'git', url: repositoryUrl },
    homepage: 'https://github.com/PlateerLab/xgeny-cli#readme',
    engines: { node: nodeEngine },
    ...(specification ? { os: [specification.os], cpu: [specification.cpu] } : {}),
    files: ['README.md', 'LICENSE'],
    publishConfig: {
      access: 'public',
      registry: 'https://registry.npmjs.org/',
    },
  };
}

async function pack(outputDirectory, name, specification, outputName) {
  const output = path.join(outputDirectory, outputName);
  try {
    await lstat(output);
    throw new Error(`refusing to replace existing package: ${output}`);
  } catch (error) {
    if (error.code !== 'ENOENT') throw error;
  }
  const temporary = await mkdtemp(path.join(os.tmpdir(), 'xgeny-npm-bootstrap.'));
  try {
    const stage = path.join(temporary, 'package');
    const packed = path.join(temporary, 'packed');
    await Promise.all([mkdir(stage), mkdir(packed)]);
    await writeFile(
      path.join(stage, 'package.json'),
      `${JSON.stringify(manifest(name, specification), null, 2)}\n`,
      'utf8',
    );
    await writeFile(
      path.join(stage, 'README.md'),
      `# \`${name}\` bootstrap\n\n` +
        'This non-executable version only creates the public package name before configuring ' +
        'the npm Trusted Publisher. Install a later Developer Preview version instead.\n',
      'utf8',
    );
    await copyFile(path.join(repoRoot, 'LICENSE'), path.join(stage, 'LICENSE'));
    const npm = npmInvocation([
      'pack',
      stage,
      '--json',
      '--ignore-scripts',
      '--pack-destination',
      packed,
    ]);
    const result = spawnSync(
      npm.command,
      npm.args,
      { encoding: 'utf8', env: { ...process.env, npm_config_audit: 'false', npm_config_fund: 'false' } },
    );
    if (result.status !== 0) {
      throw new Error(
        `npm pack failed: ${result.stderr?.trim() || result.error?.message || 'no diagnostic'}`,
      );
    }
    const [report] = JSON.parse(result.stdout);
    assert.equal(report.id, `${name}@${bootstrapVersion}`);
    assert.deepEqual(
      report.files.map(({ path: filename }) => filename).sort(),
      ['LICENSE', 'README.md', 'package.json'],
    );
    await rename(path.join(packed, report.filename), output);
    return {
      name,
      version: bootstrapVersion,
      tarball: outputName,
      integrity: report.integrity,
      shasum: report.shasum,
    };
  } finally {
    await rm(temporary, { recursive: true, force: true });
  }
}

const outputDirectory = parseArguments(process.argv.slice(2));
await mkdir(outputDirectory, { recursive: true });
const { catalog, launcher } = await distributionMetadata();
const reports = [];
for (const specification of catalog.platforms) {
  reports.push(
    await pack(
      outputDirectory,
      specification.packageName,
      specification,
      specification.tarball.replace(/\.tgz$/, '-bootstrap.tgz'),
    ),
  );
}
reports.push(await pack(outputDirectory, launcher.name, null, 'xgen-cli-bootstrap.tgz'));
const manifestPath = path.join(outputDirectory, 'npm-bootstrap-manifest.json');
await writeFile(manifestPath, `${JSON.stringify({ schemaVersion: 1, packages: reports }, null, 2)}\n`);
console.log(`npm bootstrap bundle: PASS (${reports.length} non-executable packages)`);
