import assert from 'node:assert/strict';
import { mkdtemp, rm, writeFile } from 'node:fs/promises';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import test from 'node:test';

import { assetFor, matchesAsset } from './prepare-video-extractor.mjs';

test('each installer uses its native standalone yt-dlp; macOS uses a universal executable', () => {
  assert.equal(assetFor('darwin', 'arm64'), assetFor('darwin', 'x64'));
  assert.equal(assetFor('darwin', 'arm64').name, 'yt-dlp_macos');
  assert.equal(assetFor('win32', 'x64').resource, 'video-windows/yt-dlp.exe');
  assert.equal(assetFor('linux', 'x64').name, 'yt-dlp_linux');
  assert.throws(() => assetFor('linux', 'arm64'), /No pinned/);
});

test('rejects missing, truncated and modified executables before use', async t => {
  const dir = await mkdtemp(join(tmpdir(), 'vd-video-'));
  t.after(() => rm(dir, { recursive: true, force: true }));
  const path = join(dir, 'extractor');
  const asset = { size: 3, sha256: 'ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad' };
  assert.equal(await matchesAsset(path, asset), false);
  await writeFile(path, 'abc');
  assert.equal(await matchesAsset(path, asset), true);
  await writeFile(path, 'ab');
  assert.equal(await matchesAsset(path, asset), false);
  await writeFile(path, 'xyz');
  assert.equal(await matchesAsset(path, asset), false);
});
