import { execFileSync } from 'node:child_process';
import { createHash } from 'node:crypto';
import { chmodSync, existsSync, mkdirSync, readFileSync, renameSync, writeFileSync } from 'node:fs';
import { resolve } from 'node:path';
import { fileURLToPath } from 'node:url';
import { sourceDigest } from './video-tool-sources.mjs';

const root = fileURLToPath(new URL('../', import.meta.url));

export function installerResources(platform) {
  const config = JSON.parse(readFileSync(resolve(root, `src-tauri/tauri.${platform}.conf.json`), 'utf8'));
  const dir = `binaries/video-${platform}`;
  const ext = platform === 'windows' ? '.exe' : '';
  return [...config.bundle.resources, ...['yt-dlp', 'ffmpeg', 'qjs'].map(name => `${dir}/${name}${ext}`), 'video-tool-notices'];
}

const hash = bytes => createHash('sha256').update(bytes).digest('hex');

export function buildIfChanged(target, recipe, build) {
  const stamp = `${target}.build.json`;
  if (existsSync(stamp) && existsSync(target)) {
    let saved;
    try {
      saved = JSON.parse(readFileSync(stamp, 'utf8'));
    } catch (error) {
      if (!(error instanceof SyntaxError)) throw error;
    }
    if (saved?.recipe === recipe && saved.sha256 === hash(readFileSync(target))) {
      chmodSync(target, 0o755);
      console.log(`Using verified compiled tool: ${target}`);
      return;
    }
  }
  build();
  const temporary = `${stamp}.${process.pid}.tmp`;
  writeFileSync(temporary, JSON.stringify({ recipe, sha256: hash(readFileSync(target)) }));
  renameSync(temporary, stamp);
}

function prepare(component, compiledTool, platform) {
  const script = resolve(root, `scripts/prepare-video-${component}.mjs`);
  const run = () => execFileSync(process.execPath, [script], { stdio: 'inherit' });
  if (!compiledTool) return run();

  const recipe = hash(JSON.stringify([
    platform, process.arch, hash(readFileSync(script)), sourceDigest(component === 'js' ? 'quickjs' : component),
    process.env.CC, process.env.CFLAGS, process.env.LDFLAGS,
  ]));
  const target = resolve(root, 'src-tauri/binaries', `video-${platform}`,
    compiledTool + (platform === 'windows' ? '.exe' : ''));
  buildIfChanged(target, recipe, run);
}

if (process.argv[1] && resolve(process.argv[1]) === fileURLToPath(import.meta.url)) {
  const platform = { darwin: 'macos', linux: 'linux', win32: 'windows' }[process.platform];
  if (!platform) throw new Error(`Unsupported installer platform: ${process.platform}`);
  prepare('extractor', null, platform);
  // The Windows CI consumer verifies an independently produced PE on every run.
  const prebuiltFFmpeg = process.env.VD_WINDOWS_FFMPEG_SHA256 !== undefined;
  prepare('ffmpeg', prebuiltFFmpeg ? null : 'ffmpeg', platform);
  prepare('js', platform === 'macos' ? 'qjs' : null, platform);

  // Add native tools only when making an installer. A fresh cargo test/dev
  // checkout keeps using the normal platform config without downloading tools.
  const dir = resolve(root, '.tools');
  mkdirSync(dir, { recursive: true });
  writeFileSync(resolve(dir, 'video-bundle.config.json'), JSON.stringify({
    bundle: { resources: installerResources(platform) },
  }));
}
