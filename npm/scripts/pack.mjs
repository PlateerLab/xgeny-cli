import assert from 'node:assert/strict';
import { spawnSync } from 'node:child_process';
import {
  chmod,
  copyFile,
  cp,
  lstat,
  mkdir,
  mkdtemp,
  readFile,
  rename,
  rm,
  writeFile,
} from 'node:fs/promises';
import os from 'node:os';
import path from 'node:path';

import {
  distributionMetadata,
  launcherRoot,
  npmCommand,
  platformPackageJson,
  repoRoot,
} from './config.mjs';

function parseArguments(argv) {
  const result = {};
  for (let index = 0; index < argv.length; index += 1) {
    const argument = argv[index];
    if (argument === '--launcher') {
      result.launcher = true;
      continue;
    }
    if (['--target', '--binary', '--output-dir'].includes(argument)) {
      const value = argv[index + 1];
      assert(value && !value.startsWith('--'), `${argument} requires one value`);
      result[argument.slice(2).replace('-', '_')] = value;
      index += 1;
      continue;
    }
    throw new Error(`unknown argument: ${argument}`);
  }
  assert(result.output_dir, '--output-dir is required');
  assert(Boolean(result.launcher) !== Boolean(result.target), 'select --launcher or --target');
  assert(result.launcher || result.binary, '--binary is required for a platform package');
  return result;
}

function platformReadme(specification, version) {
  return `# \`${specification.packageName}\`\n\n` +
    `Native XGENy CLI ${version} binary for \`${specification.target}\`.\n\n` +
    'This package is an exact-version optional dependency of `@xgen/cli`. ' +
    'Install the launcher package instead of installing this package directly. ' +
    'It contains no install lifecycle scripts and performs no network download.\n';
}

async function stageLauncher(stage) {
  await cp(launcherRoot, stage, { recursive: true, errorOnExist: true });
  await copyFile(path.join(repoRoot, 'LICENSE'), path.join(stage, 'LICENSE'));
  await chmod(path.join(stage, 'bin', 'xgeny.cjs'), 0o755);
}

async function stagePlatform(stage, specification, version, binary) {
  const binaryEntry = await lstat(binary);
  assert.equal(binaryEntry.isFile(), true, 'native binary must be a regular file');
  assert.equal(binaryEntry.isSymbolicLink(), false, 'native binary must not be a symbolic link');
  await mkdir(path.join(stage, 'bin'), { recursive: true });
  await writeFile(
    path.join(stage, 'package.json'),
    `${JSON.stringify(platformPackageJson(specification, version), null, 2)}\n`,
    'utf8',
  );
  await writeFile(path.join(stage, 'README.md'), platformReadme(specification, version), 'utf8');
  await copyFile(path.join(repoRoot, 'LICENSE'), path.join(stage, 'LICENSE'));
  await copyFile(
    path.join(repoRoot, 'crates', 'xgeny-cli', 'licenses', 'NATIVE_RUNTIME_PROVENANCE.md'),
    path.join(stage, 'NATIVE_RUNTIME_PROVENANCE.md'),
  );
  await copyFile(
    path.join(repoRoot, 'THIRD_PARTY_LICENSES.txt'),
    path.join(stage, 'THIRD_PARTY_LICENSES.txt'),
  );
  const destination = path.join(stage, specification.binaryPath);
  await copyFile(binary, destination);
  await chmod(destination, 0o755);
}

async function main() {
  const options = parseArguments(process.argv.slice(2));
  const { catalog, launcher, version } = await distributionMetadata();
  const specification = options.launcher
    ? null
    : catalog.platforms.find(({ target }) => target === options.target);
  assert(options.launcher || specification, `unknown target: ${options.target}`);
  const outputName = options.launcher ? 'xgen-cli.tgz' : specification.tarball;
  const expectedName = options.launcher ? launcher.name : specification.packageName;
  const outputDirectory = path.resolve(options.output_dir);
  const output = path.join(outputDirectory, outputName);
  await mkdir(outputDirectory, { recursive: true });
  try {
    await lstat(output);
    throw new Error(`refusing to replace existing package: ${output}`);
  } catch (error) {
    if (error.code !== 'ENOENT') throw error;
  }

  const temporary = await mkdtemp(path.join(os.tmpdir(), 'xgeny-npm-pack.'));
  try {
    const stage = path.join(temporary, 'package');
    const packed = path.join(temporary, 'packed');
    await mkdir(packed);
    if (options.launcher) {
      await stageLauncher(stage);
    } else {
      await stagePlatform(stage, specification, version, path.resolve(options.binary));
    }

    const result = spawnSync(
      npmCommand(),
      ['pack', stage, '--json', '--ignore-scripts', '--pack-destination', packed],
      { encoding: 'utf8', env: { ...process.env, npm_config_audit: 'false', npm_config_fund: 'false' } },
    );
    if (result.status !== 0) {
      throw new Error(`npm pack failed: ${result.stderr.trim() || 'no diagnostic'}`);
    }
    const reports = JSON.parse(result.stdout);
    assert.equal(reports.length, 1, 'npm pack must produce one package');
    const report = reports[0];
    assert.equal(report.id, `${expectedName}@${version}`);
    assert.equal(report.name, expectedName);
    assert.equal(report.version, version);
    assert.match(report.integrity, /^sha512-[A-Za-z0-9+/]+={0,2}$/);
    assert.match(report.shasum, /^[0-9a-f]{40}$/);
    assert.ok(report.files.some(({ path: filename }) => filename === 'package.json'));
    assert.equal(report.files.some(({ path: filename }) => filename.includes('node_modules')), false);
    assert.equal(report.files.some(({ path: filename }) => filename.endsWith('.tgz')), false);
    await rename(path.join(packed, report.filename), output);
    const packageJson = JSON.parse(await readFile(path.join(stage, 'package.json'), 'utf8'));
    console.log(JSON.stringify({
      name: packageJson.name,
      version: packageJson.version,
      target: specification?.target ?? null,
      tarball: output,
      integrity: report.integrity,
      shasum: report.shasum,
      size: report.size,
      unpackedSize: report.unpackedSize,
      fileCount: report.entryCount,
    }));
  } finally {
    await rm(temporary, { recursive: true, force: true });
  }
}

await main();
