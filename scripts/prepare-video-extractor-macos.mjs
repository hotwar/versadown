import { execFileSync } from 'node:child_process';
import { createHash } from 'node:crypto';
import { createReadStream } from 'node:fs';
import { chmod, lstat, mkdir, mkdtemp, readdir, rename, rm } from 'node:fs/promises';
import { dirname, join, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';
import { matchesAsset, preparePinnedAsset, version } from './prepare-video-extractor.mjs';

export { version };
// Official universal one-directory PyInstaller distribution (arm64 and x64).
export const macExtractor = {
  name: 'yt-dlp_macos.zip',
  resource: 'video-macos/yt-dlp-macos',
  size: 54165284,
  sha256: '20d98fa608998eff87d1d35a6e9fb65e4b874639a6cc89addc19da3becde3ef1',
  treeSha256: 'eb89568a3c1ecb6d3120ec22d555aa203b42e3c4bc7956e3b96a9b860fc422c7',
};

export async function digestDirectory(root) {
  const tree = createHash('sha256');
  async function visit(prefix) {
    for (const name of (await readdir(join(root, prefix))).sort()) {
      const relative = join(prefix, name);
      const path = join(root, relative);
      const info = await lstat(path);
      tree.update(relative + '\0');
      if (info.isDirectory()) {
        tree.update('directory\0');
        await visit(relative);
      } else if (info.isFile()) {
        const digest = createHash('sha256');
        for await (const bytes of createReadStream(path)) digest.update(bytes);
        tree.update(digest.digest('hex') + '\0');
      } else {
        throw new Error(`Unexpected yt-dlp bundle entry: ${relative}`);
      }
    }
  }
  if (!(await lstat(root)).isDirectory()) return null;
  await visit('');
  return tree.digest('hex');
}

export async function matchesMacExtractor(directory) {
  try {
    return await digestDirectory(directory) === macExtractor.treeSha256;
  } catch (error) {
    if (error.code === 'ENOENT' || error.code === 'ENOTDIR') return false;
    throw error;
  }
}

export async function prepareMacExtractor(directory, archive) {
  const executable = join(directory, 'yt-dlp');
  if (await matchesMacExtractor(directory)) {
    await chmod(executable, 0o755);
    return executable;
  }
  const asset = { ...macExtractor, url: `https://github.com/yt-dlp/yt-dlp/releases/download/${version}/${macExtractor.name}` };
  if (!await matchesAsset(archive, asset)) await preparePinnedAsset(archive, asset);
  await mkdir(dirname(directory), { recursive: true });
  const staging = await mkdtemp(join(dirname(directory), '.yt-dlp-extract-'));
  try {
    execFileSync('/usr/bin/ditto', ['-x', '-k', archive, staging]);
    await rename(join(staging, 'yt-dlp_macos'), join(staging, 'yt-dlp'));
    if (!await matchesMacExtractor(staging)) throw new Error('yt-dlp macOS package contents failed verification');
    await chmod(join(staging, 'yt-dlp'), 0o755);
    if (!await matchesMacExtractor(directory)) {
      await rm(directory, { recursive: true, force: true });
      try {
        await rename(staging, directory);
      } catch (error) {
        if (!['EEXIST', 'ENOTEMPTY'].includes(error.code) || !await matchesMacExtractor(directory)) throw error;
      }
    }
    return executable;
  } finally {
    await rm(staging, { recursive: true, force: true });
  }
}

if (process.argv[1] && resolve(process.argv[1]) === fileURLToPath(import.meta.url)) {
  const path = fileURLToPath(new URL(`../src-tauri/binaries/${macExtractor.resource}`, import.meta.url));
  const archive = fileURLToPath(new URL(`../.tools/video-sources/${macExtractor.name}`, import.meta.url));
  const target = await prepareMacExtractor(path, archive);
  const actual = execFileSync(target, ['--ignore-config', '--no-plugin-dirs', '--version'],
    { encoding: 'utf8', timeout: 60000 }).trim();
  if (actual !== version) throw new Error(`yt-dlp version mismatch: expected ${version}, got ${actual}`);
  console.log(`Verified ${macExtractor.resource} (${version}, ${macExtractor.size} archive bytes)`);
}
