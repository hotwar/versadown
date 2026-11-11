import assert from 'node:assert/strict';
import { spawnSync } from 'node:child_process';
import {
  chmodSync,
  copyFileSync,
  existsSync,
  mkdirSync,
  mkdtempSync,
  readFileSync,
  rmSync,
  writeFileSync,
} from 'node:fs';
import { tmpdir } from 'node:os';
import { dirname, join } from 'node:path';
import { fileURLToPath } from 'node:url';
import test from 'node:test';

const sourceScript = join(dirname(fileURLToPath(import.meta.url)), 'build-linux-release.sh');

function createFixture(t) {
  assert.ok(existsSync(sourceScript), 'Linux release script must exist');
  const root = mkdtempSync(join(tmpdir(), 'versadown-linux-release-'));
  t.after(() => rmSync(root, { recursive: true, force: true }));

  const script = join(root, 'scripts', 'build-linux-release.sh');
  const sidecar = join(root, 'src-tauri', 'binaries', 'aria2-next-x86_64-unknown-linux-gnu');
  const fakePnpm = join(root, 'bin', 'pnpm');
  const fakeUname = join(root, 'bin', 'uname');
  for (const path of [script, sidecar, fakePnpm, fakeUname]) mkdirSync(dirname(path), { recursive: true });
  copyFileSync(sourceScript, script);
  writeFileSync(join(root, 'scripts', 'prepare-video-tools.mjs'), '');
  writeFileSync(sidecar, '#!/bin/sh\nexit 0\n');
  chmodSync(sidecar, 0o755);
  writeFileSync(fakePnpm, `#!/usr/bin/env node
const fs = require('node:fs');
const path = require('node:path');
const root = process.env.CARGO_TARGET_DIR;
fs.writeFileSync(path.join(process.env.FIXTURE_ROOT, 'pnpm-args'), process.argv.slice(2).join(' '));
for (const [kind, name] of [['deb', 'VersaDown_0.1.0_amd64.deb'], ['rpm', 'VersaDown-0.1.0-1.x86_64.rpm'], ['appimage', 'VersaDown_0.1.0_amd64.AppImage']]) {
  if (kind === 'appimage' && process.env.FAKE_DEB_ONLY === '1') continue;
  const folder = path.join(root, 'release', 'bundle', kind);
  fs.mkdirSync(folder, { recursive: true });
  fs.writeFileSync(path.join(folder, name), kind + '-payload');
}
`);
  chmodSync(fakePnpm, 0o755);
  writeFileSync(fakeUname, `#!/usr/bin/env bash
case "$1" in
  -s) echo Linux ;;
  -m) echo x86_64 ;;
  *) exit 1 ;;
esac
`);
  chmodSync(fakeUname, 0o755);
  return { root, script };
}

function runFixture({ root, script }, extraEnv = {}) {
  return spawnSync('bash', [script], {
    cwd: root,
    encoding: 'utf8',
    env: { ...process.env, ...extraEnv, FIXTURE_ROOT: root, PATH: `${join(root, 'bin')}:${process.env.PATH}` },
  });
}

test('builds only Linux bundles and publishes all packages with valid checksums', (t) => {
  const fixture = createFixture(t);
  const result = runFixture(fixture);
  assert.equal(result.status, 0, result.stderr);
  assert.equal(readFileSync(join(fixture.root, 'pnpm-args'), 'utf8'), 'exec tauri build --bundles deb,rpm,appimage --config .tools/video-bundle.config.json --ci');

  const output = join(fixture.root, 'artifacts', 'linux');
  const deb = 'VersaDown_0.1.0_amd64.deb';
  const rpm = 'VersaDown-0.1.0-1.x86_64.rpm';
  const appImage = 'VersaDown_0.1.0_amd64.AppImage';
  assert.equal(readFileSync(join(output, deb), 'utf8'), 'deb-payload');
  assert.equal(readFileSync(join(output, rpm), 'utf8'), 'rpm-payload');
  assert.equal(readFileSync(join(output, appImage), 'utf8'), 'appimage-payload');
  // sha256sum localizes its verdict ("成功" on zh_CN); pin the C locale.
  const checksum = spawnSync('sha256sum', ['--check', 'SHA256SUMS'], {
    cwd: output,
    encoding: 'utf8',
    env: { ...process.env, LC_ALL: 'C' },
  });
  assert.equal(checksum.status, 0, checksum.stderr);
  assert.match(checksum.stdout, /VersaDown_0\.1\.0_amd64\.deb: OK/);
  assert.match(checksum.stdout, /VersaDown-0\.1\.0-1\.x86_64\.rpm: OK/);
  assert.match(checksum.stdout, /VersaDown_0\.1\.0_amd64\.AppImage: OK/);
});

test('rejects incomplete bundles even if an older AppImage is present', (t) => {
  const fixture = createFixture(t);
  const staleDir = join(fixture.root, '.tools', 'linux-release-target', 'release', 'bundle', 'appimage');
  mkdirSync(staleDir, { recursive: true });
  writeFileSync(join(staleDir, 'stale.AppImage'), 'old');

  const result = runFixture(fixture, { FAKE_DEB_ONLY: '1' });
  assert.notEqual(result.status, 0);
  assert.match(result.stderr, /AppImage/);
  assert.equal(existsSync(join(fixture.root, 'artifacts', 'linux', 'SHA256SUMS')), false);
});
