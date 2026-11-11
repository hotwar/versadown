import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import { join } from 'node:path';
import { fileURLToPath } from 'node:url';
import test from 'node:test';

const root = fileURLToPath(new URL('../src-tauri/', import.meta.url));
const config = (name) => JSON.parse(readFileSync(join(root, name), 'utf8'));

test('platform bundles include only their own aria2 executable', () => {
  assert.deepEqual(config('tauri.conf.json').bundle.resources ?? [], []);
  assert.deepEqual(config('tauri.windows.conf.json').bundle.resources, [
    'binaries/aria2-next-x86_64-pc-windows-msvc.exe',
  ]);
  assert.deepEqual(config('tauri.linux.conf.json').bundle.resources, [
    'binaries/aria2-next-x86_64-unknown-linux-gnu',
  ]);
  assert.deepEqual(config('tauri.macos.conf.json').bundle.resources, [
    'binaries/aria2-next-aarch64-apple-darwin',
    'binaries/aria2-next-x86_64-apple-darwin',
  ]);
});

test('installers add only host video tools and their notices to existing platform resources', async () => {
  const { installerResources } = await import('./prepare-video-tools.mjs');
  for (const platform of ['linux', 'windows', 'macos']) {
    const ext = platform === 'windows' ? '.exe' : '';
    assert.deepEqual(installerResources(platform), [
      ...config(`tauri.${platform}.conf.json`).bundle.resources,
      ...['yt-dlp', 'ffmpeg', 'qjs'].map(name => `binaries/video-${platform}/${name}${ext}`),
      'video-tool-notices',
    ]);
  }
});

test('compiled video tool cache requires both the current recipe and intact binary', async t => {
  const { buildIfChanged } = await import('./prepare-video-tools.mjs');
  const { mkdtempSync, rmSync, writeFileSync } = await import('node:fs');
  const { tmpdir } = await import('node:os');
  const dir = mkdtempSync(join(tmpdir(), 'vd-tool-cache-'));
  t.after(() => rmSync(dir, { recursive: true, force: true }));
  const target = join(dir, 'ffmpeg');
  let builds = 0;
  const build = () => writeFileSync(target, `native binary ${++builds}`);
  buildIfChanged(target, 'recipe A', build);
  buildIfChanged(target, 'recipe A', build);
  assert.equal(builds, 1);
  writeFileSync(target, 'corrupt binary');
  buildIfChanged(target, 'recipe A', build);
  assert.equal(builds, 2);
  buildIfChanged(target, 'recipe B', build);
  assert.equal(builds, 3);
});
