import { execFileSync } from 'node:child_process';
import { createHash } from 'node:crypto';
import { chmod, mkdir, mkdtemp, readFile, rename, rm, stat } from 'node:fs/promises';
import { basename, dirname, join, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';

export const version = '2026.10.26';
// Check these pins against the release's signed SHA2-256SUMS before updating.
const assets = {
  darwin: {
    name: 'yt-dlp_macos', resource: 'video-macos/yt-dlp', size: 37466304,
    sha256: '0f922c15ccc0b83eefa1c8bc9104e434318c4bf16e408e1ee93b7745a6efe0aa',
  },
  linux: {
    name: 'yt-dlp_linux', resource: 'video-linux/yt-dlp', size: 41166968,
    sha256: '767ba9dce7052aa196f8b12652f433d76de128ba168966fa84b6394d5e6c6795',
  },
  win32: {
    name: 'yt-dlp.exe', resource: 'video-windows/yt-dlp.exe', size: 23575744,
    sha256: 'f5389d3698ecde4ef06f4e6b1ce0294fe9ac1396228c8990b8d3416d6d177a7d',
  },
};

export function assetFor(platform, arch) {
  if (Object.hasOwn(assets, platform) && (arch === 'x64' || (platform === 'darwin' && arch === 'arm64'))) {
    return assets[platform];
  }
  throw new Error(`No pinned yt-dlp binary for ${platform}/${arch}`);
}

export async function matchesAsset(file, asset) {
  const info = await stat(file).catch(error => {
    if (error.code === 'ENOENT') return null;
    throw error;
  });
  return info?.isFile() === true && info.size === asset.size
    && createHash('sha256').update(await readFile(file)).digest('hex') === asset.sha256;
}

export async function preparePinnedAsset(target, asset) {
  if (await matchesAsset(target, asset)) return target;
  await mkdir(dirname(target), { recursive: true });
  const temp = await mkdtemp(join(dirname(target), '.prepare-'));
  try {
    const file = join(temp, basename(target));
    const sources = [asset.url, ...(asset.mirrors ?? [])];
    for (const [index, url] of sources.entries()) {
      try {
        execFileSync('curl', ['--disable', '--fail', '--location', '--show-error', '--silent',
          '--proto', '=https', '--proto-redir', '=https', '--retry', '2', '--connect-timeout', '15',
          '--max-time', '240', '--output', file, url], { stdio: 'inherit' });
        break;
      } catch (error) {
        if (!error.status || index === sources.length - 1) throw error;
        console.warn(`Download failed at ${new URL(url).host}; trying the next source for ${asset.name}`);
      }
    }
    // The same pin covers every source. A successful transfer with different
    // bytes is an integrity error, never a reason to try another mirror.
    if (!await matchesAsset(file, asset)) throw new Error(`Integrity check failed for ${asset.name}`);
    await chmod(file, 0o755);
    await rename(file, target);
    return target;
  } finally {
    await rm(temp, { recursive: true, force: true });
  }
}

async function prepare() {
  const asset = assetFor(process.platform, process.arch);
  const binaries = fileURLToPath(new URL('../src-tauri/binaries/', import.meta.url));
  const target = await preparePinnedAsset(join(binaries, asset.resource), {
    ...asset, url: `https://github.com/yt-dlp/yt-dlp/releases/download/${version}/${asset.name}`,
  });
  const actual = execFileSync(target, ['--ignore-config', '--no-plugin-dirs', '--version'],
    { encoding: 'utf8', timeout: 60000 }).trim();
  if (actual !== version) throw new Error(`yt-dlp version mismatch: expected ${version}, got ${actual}`);
  console.log(`Verified ${asset.resource} (${version}, ${asset.size} bytes)`);
}

if (process.argv[1] && resolve(process.argv[1]) === fileURLToPath(import.meta.url)) {
  await prepare();
}
