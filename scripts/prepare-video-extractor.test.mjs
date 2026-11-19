import assert from 'node:assert/strict';
import { mkdir, mkdtemp, rm, writeFile } from 'node:fs/promises';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import test from 'node:test';

import { assetFor, matchesAsset } from './prepare-video-extractor.mjs';
import { digestDirectory, macExtractor } from './prepare-video-extractor-macos.mjs';

test('macOS installers use the official directory archive with a pinned tree digest', () => {
  assert.equal(macExtractor.name, 'yt-dlp_macos.zip');
  assert.match(macExtractor.sha256, /^[a-f0-9]{64}$/);
  assert.match(macExtractor.treeSha256, /^[a-f0-9]{64}$/);
});

test('historical macOS and other platforms can still read the standalone asset metadata', () => {
  assert.equal(assetFor('darwin', 'arm64'), assetFor('darwin', 'x64'));
  assert.equal(assetFor('darwin', 'arm64').name, 'yt-dlp_macos');
  assert.equal(assetFor('win32', 'x64').resource, 'video-windows/yt-dlp.exe');
  assert.equal(assetFor('linux', 'x64').name, 'yt-dlp_linux');
  assert.throws(() => assetFor('linux', 'arm64'), /No pinned/);
});

test('directory integrity includes the launcher and its dependencies', async t => {
  const dir = await mkdtemp(join(tmpdir(), 'vd-video-directory-'));
  t.after(() => rm(dir, { recursive: true, force: true }));
  const internal = join(dir, '_internal');
  await mkdir(internal);
  await writeFile(join(dir, 'yt-dlp'), 'launcher');
  const dependency = join(internal, 'library');
  await writeFile(dependency, 'verified dependency');
  const digest = await digestDirectory(dir);
  await writeFile(dependency, 'different dependency');
  assert.notEqual(await digestDirectory(dir), digest);
  await rm(dependency);
  assert.notEqual(await digestDirectory(dir), digest);
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
