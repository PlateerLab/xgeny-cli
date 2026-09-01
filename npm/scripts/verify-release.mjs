import assert from 'node:assert/strict';
import { spawn } from 'node:child_process';
import { createHash } from 'node:crypto';
import { lstat, readFile } from 'node:fs/promises';
import path from 'node:path';
import { once } from 'node:events';
import { fileURLToPath } from 'node:url';

import {
  distributionMetadata,
  launcherRoot,
  npmInvocation,
  platformPackageJson,
  repoRoot,
} from './config.mjs';

function parseArguments(argv) {
  const result = {};
  for (let index = 0; index < argv.length; index += 2) {
    const option = argv[index];
    const value = argv[index + 1];
    assert(['--directory', '--tag'].includes(option) && value, `invalid argument near ${option}`);
    result[option.slice(2)] = value;
  }
  assert(result.directory && result.tag, '--directory and --tag are required');
  return { directory: path.resolve(result.directory), tag: result.tag };
}

async function run(command, arguments_, options = {}) {
  const child = spawn(command, arguments_, {
    ...options,
    shell: false,
    stdio: ['ignore', 'pipe', 'pipe'],
    windowsHide: true,
  });
  const stdout = [];
  const stderr = [];
  let bytes = 0;
  const append = (destination, chunk) => {
    bytes += chunk.length;
    if (bytes > 4 * 1024 * 1024) child.kill();
    else destination.push(chunk);
  };
  child.stdout.on('data', (chunk) => append(stdout, chunk));
  child.stderr.on('data', (chunk) => append(stderr, chunk));
  const [code, signal] = await once(child, 'exit');
  const result = {
    code,
    signal,
    stdout: Buffer.concat(stdout),
    stderr: Buffer.concat(stderr),
  };
  if (code !== 0 || signal) {
    throw new Error(
      `${command} failed (${signal ?? code}): ${result.stderr.toString('utf8').trim()}`,
    );
  }
  return result;
}

async function tarMember(tarball, member) {
  return (await run('tar', ['-xOf', tarball, `package/${member}`])).stdout;
}

async function hashFile(filename, algorithm = 'sha256') {
  return createHash(algorithm).update(await readFile(filename)).digest('hex');
}

async function hashMember(tarball, member, algorithm = 'sha256') {
  const hash = createHash(algorithm);
  const child = spawn('tar', ['-xOf', tarball, `package/${member}`], {
    shell: false,
    stdio: ['ignore', 'pipe', 'pipe'],
    windowsHide: true,
  });
  const stderr = [];
  let stderrBytes = 0;
  child.stdout.on('data', (chunk) => hash.update(chunk));
  child.stderr.on('data', (chunk) => {
    stderrBytes += chunk.length;
    if (stderrBytes > 64 * 1024) child.kill();
    else stderr.push(chunk);
  });
  const [code, signal] = await once(child, 'exit');
  if (code !== 0 || signal) {
    throw new Error(
      `tar failed (${signal ?? code}): ${Buffer.concat(stderr).toString('utf8').trim()}`,
    );
  }
  return hash.digest('hex');
}

export function normalizeDryRunReport(parsed) {
  if (parsed && typeof parsed === 'object' && !Array.isArray(parsed) && parsed.id) {
    return parsed;
  }
  assert(parsed && typeof parsed === 'object' && !Array.isArray(parsed));
  const reports = Object.values(parsed);
  assert.equal(reports.length, 1, 'npm publish dry-run must describe exactly one package');
  assert(reports[0] && typeof reports[0].id === 'string');
  return reports[0];
}

async function dryRun(tarball, distTag) {
  const npm = npmInvocation([
    'publish',
    '--dry-run',
    '--ignore-scripts',
    '--tag',
    distTag,
    '--json',
    tarball,
  ]);
  const result = await run(
    npm.command,
    npm.args,
    { env: { ...process.env, npm_config_audit: 'false', npm_config_fund: 'false' } },
  );
  return normalizeDryRunReport(JSON.parse(result.stdout.toString('utf8')));
}

export async function verifyRelease(directory, tag) {
  const { catalog, launcher, version } = await distributionMetadata();
  assert.match(tag, /^v(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)(?:-[0-9A-Za-z.-]+)?$/);
  assert.equal(tag, `v${version}`);
  const distTag = version.includes('-') ? 'next' : 'latest';
  const launcherTarball = path.join(directory, 'xgen-cli.tgz');
  const expectedTarballs = [launcherTarball];
  for (const specification of catalog.platforms) {
    expectedTarballs.push(path.join(directory, specification.tarball));
  }
  for (const tarball of expectedTarballs) {
    const entry = await lstat(tarball);
    assert.equal(entry.isFile(), true, `${path.basename(tarball)} must be a regular file`);
    assert.equal(entry.isSymbolicLink(), false, `${path.basename(tarball)} must not be a symlink`);
  }

  const launcherReport = await dryRun(launcherTarball, distTag);
  assert.equal(launcherReport.id, `${launcher.name}@${version}`);
  assert.deepEqual(JSON.parse((await tarMember(launcherTarball, 'package.json')).toString('utf8')), launcher);
  assert.deepEqual(
    JSON.parse((await tarMember(launcherTarball, 'platforms.json')).toString('utf8')),
    (await distributionMetadata()).catalog,
  );
  assert.equal(
    await hashMember(launcherTarball, 'bin/xgeny.cjs'),
    await hashFile(path.join(launcherRoot, 'bin', 'xgeny.cjs')),
  );
  assert.deepEqual(
    launcherReport.files.map(({ path: filename }) => filename).sort(),
    ['LICENSE', 'README.md', 'bin/xgeny.cjs', 'lib/platform.cjs', 'package.json', 'platforms.json'],
  );

  const reports = [];
  for (const specification of catalog.platforms) {
    const tarball = path.join(directory, specification.tarball);
    const report = await dryRun(tarball, distTag);
    assert.equal(report.id, `${specification.packageName}@${version}`);
    assert.deepEqual(
      JSON.parse((await tarMember(tarball, 'package.json')).toString('utf8')),
      platformPackageJson(specification, version),
    );
    assert.deepEqual(
      report.files.map(({ path: filename }) => filename).sort(),
      [
        'LICENSE',
        'NATIVE_RUNTIME_PROVENANCE.md',
        'README.md',
        'THIRD_PARTY_LICENSES.txt',
        specification.binaryPath,
        'package.json',
      ].sort(),
    );
    assert.equal(
      await hashMember(tarball, specification.binaryPath),
      await hashFile(path.join(directory, specification.asset)),
      `${specification.packageName} must contain the exact GitHub release binary`,
    );
    for (const [member, source] of [
      ['LICENSE', path.join(repoRoot, 'LICENSE')],
      [
        'NATIVE_RUNTIME_PROVENANCE.md',
        path.join(repoRoot, 'crates', 'xgeny-cli', 'licenses', 'NATIVE_RUNTIME_PROVENANCE.md'),
      ],
      ['THIRD_PARTY_LICENSES.txt', path.join(repoRoot, 'THIRD_PARTY_LICENSES.txt')],
    ]) {
      assert.equal(await hashMember(tarball, member), await hashFile(source));
    }
    reports.push({ name: specification.packageName, tarball, integrity: report.integrity });
  }
  reports.push({ name: launcher.name, tarball: launcherTarball, integrity: launcherReport.integrity });
  console.log(`npm release bundle: PASS (${version}, ${reports.length} packages)`);
  return { catalog, launcher, reports, version };
}

if (fileURLToPath(import.meta.url) === path.resolve(process.argv[1])) {
  const { directory, tag } = parseArguments(process.argv.slice(2));
  await verifyRelease(directory, tag);
}
