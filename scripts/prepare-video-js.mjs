import { execFileSync } from 'node:child_process';
import { chmod, copyFile, mkdir, mkdtemp, rename, rm } from 'node:fs/promises';
import { tmpdir } from 'node:os';
import { dirname, join } from 'node:path';
import { fileURLToPath } from 'node:url';
import { preparePinnedAsset } from './prepare-video-extractor.mjs';
import { sourceArchive } from './video-tool-sources.mjs';

const root = fileURLToPath(new URL('../', import.meta.url));
const platform = { darwin: 'macos', linux: 'linux', win32: 'windows' }[process.platform];
const native = {
  linux: {
    name: 'qjs-linux-x86_64', size: 2534024,
    sha256: '85ded23b9fabe8c1644fc40a8d2bbdd71157affcbd807359c618cc717ee24d7d',
  },
  windows: {
    name: 'qjs-windows-x86_64.exe', size: 1873152,
    sha256: 'ff03398d74ea6e6bd3a8e58058f7003225abcac229d7d57775b7aaae812a49c7',
  },
};

async function buildForMac(target) {
  // Official QuickJS-NG v0.18.0 Mach-O binaries target macOS 26.
  const archive = await sourceArchive('quickjs');
  const temp = await mkdtemp(join(tmpdir(), 'versadown-quickjs-'));
  const staging = `${target}.${process.pid}.tmp`;
  try {
    const source = join(temp, 'src');
    const build = join(temp, 'build');
    const bin = join(temp, 'bin');
    await mkdir(source);
    execFileSync('tar', ['-xzf', archive, '--strip-components', '1', '-C', source]);
    execFileSync('cmake', ['-S', source, '-B', build, '-DCMAKE_BUILD_TYPE=Release',
      '-DBUILD_SHARED_LIBS=OFF', '-DCMAKE_OSX_DEPLOYMENT_TARGET=12.0',
      `-DCMAKE_RUNTIME_OUTPUT_DIRECTORY=${bin}`], { stdio: 'inherit' });
    execFileSync('cmake', ['--build', build, '--config', 'Release', '--target', 'qjs_exe', '--parallel', '4'],
      { stdio: 'inherit' });
    await mkdir(dirname(target), { recursive: true });
    await copyFile(join(bin, 'qjs'), staging);
    await chmod(staging, 0o755);
    await rename(staging, target);
  } finally {
    await rm(staging, { force: true });
    await rm(temp, { recursive: true, force: true });
  }
}

if (!platform || !(process.arch === 'x64' || (platform === 'macos' && process.arch === 'arm64'))) {
  throw new Error(`Unsupported QuickJS platform: ${process.platform}/${process.arch}`);
}
const target = join(root, 'src-tauri', 'binaries', `video-${platform}`, platform === 'windows' ? 'qjs.exe' : 'qjs');
if (platform === 'macos') {
  await buildForMac(target);
} else {
  const asset = native[platform];
  await preparePinnedAsset(target, {
    ...asset, url: `https://github.com/quickjs-ng/quickjs/releases/download/v0.18.0/${asset.name}`,
  });
}
const result = execFileSync(target, ['-e', 'console.log(21 * 2)'], { encoding: 'utf8', timeout: 30000 }).trim();
if (result !== '42') throw new Error('Packaged QuickJS failed its script execution check');
