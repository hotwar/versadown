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
