import assert from 'node:assert/strict';
import { spawn } from 'node:child_process';
import { createHash } from 'node:crypto';
import {
  lstat,
  mkdir,
  mkdtemp,
  readFile,
  rm,
} from 'node:fs/promises';
import http from 'node:http';
import { createRequire } from 'node:module';
import os from 'node:os';
import path from 'node:path';
import { once } from 'node:events';

import {
  distributionMetadata,
  npmInvocation,
  platformPackageJson,
} from './config.mjs';

function parseArguments(argv) {
  const result = {};
  for (let index = 0; index < argv.length; index += 2) {
    const option = argv[index];
    const value = argv[index + 1];
    assert(
      ['--launcher', '--platform', '--target', '--expected-version'].includes(option) && value,
      `invalid argument near ${option ?? '<end>'}`,
    );
    result[option.slice(2).replace('-', '_')] = value;
  }
  for (const required of ['launcher', 'platform', 'target']) {
    assert(result[required], `--${required.replace('_', '-')} is required`);
  }
  return result;
}

async function packageArtifact(filename, manifest, registry) {
  const bytes = await readFile(filename);
  const basename = path.basename(filename);
  return {
    bytes,
    manifest: {
      ...manifest,
      _id: `${manifest.name}@${manifest.version}`,
      dist: {
        integrity: `sha512-${createHash('sha512').update(bytes).digest('base64')}`,
        shasum: createHash('sha1').update(bytes).digest('hex'),
        tarball: `${registry}/tarballs/${encodeURIComponent(basename)}`,
      },
    },
  };
}

function packument(artifact) {
  return {
    _id: artifact.manifest.name,
    name: artifact.manifest.name,
    'dist-tags': { latest: artifact.manifest.version },
    versions: { [artifact.manifest.version]: artifact.manifest },
  };
}

async function run(command, arguments_, options = {}) {
  const child = spawn(command, arguments_, {
    ...options,
    env: options.env ?? process.env,
    shell: false,
    stdio: ['ignore', 'pipe', 'pipe'],
    windowsHide: true,
  });
  const stdout = [];
  const stderr = [];
  let outputBytes = 0;
  const append = (destination, chunk) => {
    outputBytes += chunk.length;
    if (outputBytes > 1024 * 1024) {
      child.kill();
      return;
    }
    destination.push(chunk);
  };
  child.stdout.on('data', (chunk) => append(stdout, chunk));
  child.stderr.on('data', (chunk) => append(stderr, chunk));
  const [code, signal] = await once(child, 'exit');
  const result = {
    code,
    signal,
    stdout: Buffer.concat(stdout).toString('utf8'),
    stderr: Buffer.concat(stderr).toString('utf8'),
  };
  if (code !== 0 || signal) {
    throw new Error(
      `${command} failed (${signal ?? code}): ${result.stderr.trim() || result.stdout.trim()}`,
    );
  }
  return result;
}

async function main() {
  const options = parseArguments(process.argv.slice(2));
  const { catalog, launcher, version } = await distributionMetadata();
  if (options.expected_version) assert.equal(options.expected_version, version);
  const specification = catalog.platforms.find(({ target }) => target === options.target);
  assert(specification, `unknown target: ${options.target}`);
  assert.equal(specification.os, process.platform, 'smoke target must match the runner OS');
  assert.equal(specification.cpu, process.arch, 'smoke target must match the runner architecture');

  const temporary = await mkdtemp(path.join(os.tmpdir(), 'xgeny-npm-smoke.'));
  const requestedTarballs = [];
  let server;
  try {
    const installRoot = path.join(temporary, 'install');
    const cache = path.join(temporary, 'cache');
    const home = path.join(temporary, 'home');
    await Promise.all([mkdir(installRoot), mkdir(cache), mkdir(home)]);

    server = http.createServer();
    server.listen(0, '127.0.0.1');
    await once(server, 'listening');
    const address = server.address();
    assert(address && typeof address === 'object');
    const registry = `http://127.0.0.1:${address.port}`;

    const artifacts = new Map();
    for (const [filename, manifest] of [
      [path.resolve(options.launcher), launcher],
      [path.resolve(options.platform), platformPackageJson(specification, version)],
    ]) {
      const artifact = await packageArtifact(filename, manifest, registry);
      artifacts.set(manifest.name, artifact);
    }

    server.on('request', (request, response) => {
      const pathname = new URL(request.url, registry).pathname;
      if (pathname.startsWith('/tarballs/')) {
        const requested = decodeURIComponent(pathname.slice('/tarballs/'.length));
        const artifact = [...artifacts.values()].find(
          ({ manifest }) => path.basename(new URL(manifest.dist.tarball).pathname) === requested,
        );
        if (artifact) {
          requestedTarballs.push(artifact.manifest.name);
          response.writeHead(200, {
            'content-type': 'application/octet-stream',
            'content-length': artifact.bytes.length,
          });
          response.end(artifact.bytes);
          return;
        }
      } else {
        const packageName = decodeURIComponent(pathname.slice(1));
        const artifact = artifacts.get(packageName);
        if (artifact) {
          const body = Buffer.from(JSON.stringify(packument(artifact)));
          response.writeHead(200, {
            'content-type': 'application/json',
            'content-length': body.length,
          });
          response.end(body);
          return;
        }
      }
      response.writeHead(404, { 'content-type': 'application/json' });
      response.end(JSON.stringify({ error: 'not_found' }));
    });

    const npmEnvironment = {
      ...process.env,
      HOME: home,
      USERPROFILE: home,
      NO_PROXY: '127.0.0.1,localhost',
      no_proxy: '127.0.0.1,localhost',
      npm_config_cache: cache,
      npm_config_registry: registry,
      npm_config_audit: 'false',
      npm_config_fund: 'false',
      npm_config_update_notifier: 'false',
    };
    const install = npmInvocation([
      'install',
      '--global',
      '--prefix',
      installRoot,
      '--ignore-scripts',
      '--no-audit',
      '--no-fund',
      '--loglevel=error',
      `${launcher.name}@${version}`,
    ]);
    await run(install.command, install.args, { env: npmEnvironment });
    server.close();
    await once(server, 'close');
    server = null;

    assert.deepEqual(new Set(requestedTarballs), new Set([launcher.name, specification.packageName]));
    const root = npmInvocation(['root', '--global', '--prefix', installRoot]);
    const rootResult = await run(root.command, root.args, { env: npmEnvironment });
    const globalRoot = rootResult.stdout.trim();
    const installedLauncher = path.join(globalRoot, '@xgen', 'cli', 'package.json');
    const launcherEntry = await lstat(installedLauncher);
    assert.equal(launcherEntry.isFile(), true);
    const requireFromLauncher = createRequire(installedLauncher);
    const installedPlatform = requireFromLauncher.resolve(`${specification.packageName}/package.json`);
    assert.equal(JSON.parse(await readFile(installedPlatform, 'utf8')).version, version);
    for (const candidate of catalog.platforms) {
      if (candidate.packageName === specification.packageName) continue;
      assert.throws(
        () => requireFromLauncher.resolve(`${candidate.packageName}/package.json`),
        (error) => error.code === 'MODULE_NOT_FOUND',
      );
    }

    const state = path.join(temporary, 'state');
    const executionEnvironment = {
      ...process.env,
      HOME: home,
      USERPROFILE: home,
      XGENY_STATE_HOME: state,
    };
    let versionResult;
    if (process.platform === 'win32') {
      const shim = path.join(installRoot, 'xgeny.cmd');
      versionResult = await run(
        process.env.ComSpec ?? 'cmd.exe',
        ['/d', '/s', '/c', `"${shim}" --version`],
        { env: executionEnvironment },
      );
    } else {
      const shim = path.join(installRoot, 'bin', 'xgeny');
      versionResult = await run(shim, ['--version'], { env: executionEnvironment });
    }
    assert.equal(versionResult.stdout.trim(), `xgeny ${version}`);
    await assert.rejects(lstat(state), (error) => error.code === 'ENOENT');
    console.log(
      `npm global install smoke: PASS (${launcher.name} -> ${specification.packageName} ${version})`,
    );
  } finally {
    if (server) {
      server.close();
      await once(server, 'close');
    }
    await rm(temporary, { recursive: true, force: true });
  }
}

await main();
