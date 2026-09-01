import assert from 'node:assert/strict';
import { spawn } from 'node:child_process';
import { createHash } from 'node:crypto';
import { readFile } from 'node:fs/promises';
import path from 'node:path';
import { once } from 'node:events';
import { setTimeout as delay } from 'node:timers/promises';
import { fileURLToPath } from 'node:url';

import { npmInvocation } from './config.mjs';
import { verifyRelease } from './verify-release.mjs';

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

async function run(arguments_, { allow404 = false } = {}) {
  const npm = npmInvocation(arguments_);
  const child = spawn(npm.command, npm.args, {
    shell: false,
    stdio: ['ignore', 'pipe', 'pipe'],
    windowsHide: true,
    env: {
      ...process.env,
      npm_config_audit: 'false',
      npm_config_fund: 'false',
      npm_config_registry: 'https://registry.npmjs.org/',
    },
  });
  const stdout = [];
  const stderr = [];
  child.stdout.on('data', (chunk) => stdout.push(chunk));
  child.stderr.on('data', (chunk) => stderr.push(chunk));
  const [code, signal] = await once(child, 'exit');
  const result = {
    code,
    signal,
    stdout: Buffer.concat(stdout).toString('utf8'),
    stderr: Buffer.concat(stderr).toString('utf8'),
  };
  if (allow404 && code !== 0 && /\bE404\b/.test(result.stderr)) return null;
  if (code !== 0 || signal) {
    throw new Error(`npm ${arguments_[0]} failed (${signal ?? code}): ${result.stderr.trim()}`);
  }
  return result;
}

async function remoteMetadata(name, version) {
  const result = await run(
    ['view', `${name}@${version}`, 'dist.integrity', 'dist.attestations', 'dist-tags', '--json'],
    { allow404: true },
  );
  return result ? JSON.parse(result.stdout) : null;
}

export function publicationOrder(reports, launcherName) {
  const launcherReports = reports.filter(({ name }) => name === launcherName);
  assert.equal(launcherReports.length, 1, 'release bundle must contain exactly one launcher');
  return [
    ...reports.filter(({ name }) => name !== launcherName),
    launcherReports[0],
  ];
}

export function validateExistingPublication(remote, report, version, distTag) {
  assert.equal(remote['dist.integrity'], report.integrity, `${report.name} already differs on npm`);
  assert.equal(remote['dist-tags'][distTag], version, `${report.name} has the wrong dist-tag`);
  assert.equal(
    remote['dist.attestations']?.provenance?.predicateType,
    'https://slsa.dev/provenance/v1',
    `${report.name} is missing npm provenance`,
  );
}

async function publishOne(report, version, distTag) {
  const bytes = await readFile(report.tarball);
  const localIntegrity = `sha512-${createHash('sha512').update(bytes).digest('base64')}`;
  assert.equal(localIntegrity, report.integrity);
  let remote = await remoteMetadata(report.name, version);
  if (remote) {
    validateExistingPublication(remote, report, version, distTag);
    console.log(`npm publish: SKIP identical ${report.name}@${version}`);
    return;
  }
  await run([
    'publish',
    '--access',
    'public',
    '--tag',
    distTag,
    '--provenance',
    '--ignore-scripts',
    report.tarball,
  ]);
  for (let attempt = 0; attempt < 12; attempt += 1) {
    remote = await remoteMetadata(report.name, version);
    if (
      remote &&
      remote['dist.integrity'] === localIntegrity &&
      remote['dist-tags']?.[distTag] === version &&
      remote['dist.attestations']?.provenance?.predicateType ===
        'https://slsa.dev/provenance/v1'
    ) {
      console.log(`npm publish: PASS ${report.name}@${version} (${distTag})`);
      return;
    }
    await delay(5000);
  }
  throw new Error(`${report.name}@${version} publication did not become verifiable`);
}

async function main() {
  const { directory, tag } = parseArguments(process.argv.slice(2));
  const { launcher, reports, version } = await verifyRelease(directory, tag);
  const distTag = version.includes('-') ? 'next' : 'latest';
  for (const report of publicationOrder(reports, launcher.name)) {
    await publishOne(report, version, distTag);
  }
}

if (fileURLToPath(import.meta.url) === path.resolve(process.argv[1])) {
  await main();
}
