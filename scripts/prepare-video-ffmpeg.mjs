import { execFileSync } from 'node:child_process';
import { createHash } from 'node:crypto';
import { chmod, copyFile, mkdir, mkdtemp, readFile, rename, rm } from 'node:fs/promises';
import { availableParallelism, tmpdir } from 'node:os';
import { dirname, join } from 'node:path';
import { fileURLToPath } from 'node:url';
import { ffmpegVersion as version, sourceArchive } from './video-tool-sources.mjs';

const root = fileURLToPath(new URL('../', import.meta.url));
const crossWindows = process.argv.includes('--cross-windows-x64');
if (crossWindows && (process.platform !== 'linux' || process.arch !== 'x64')) {
  throw new Error('The Windows cross-build requires an x86_64 Linux host');
}
const platform = crossWindows ? 'windows' : { darwin: 'macos', linux: 'linux', win32: 'windows' }[process.platform];
if (!platform) throw new Error(`Unsupported FFmpeg installer platform: ${process.platform}`);
const windows = platform === 'windows';

// yt-dlp fetches fragments. This independent LGPL executable only reads local
// streams, copies their packets and fixes container framing; it needs no encoder.
const options = [
  '--disable-autodetect', '--disable-gpl', '--disable-version3', '--disable-nonfree',
  '--enable-static', '--disable-shared', '--disable-network', '--disable-doc',
  '--disable-debug', '--disable-asm', '--disable-programs', '--disable-avdevice',
  '--disable-everything', '--enable-ffmpeg', '--enable-protocol=file,pipe',
  '--enable-demuxer=mov,matroska,mpegts,aac,mp3,ogg,flv,concat',
  '--enable-muxer=mp4,mov,ipod,webm,matroska,mpegts',
  // avformat probes codec headers even for stream copy (e.g. AAC rate in TS).
  '--enable-decoder=aac,mp3,opus,vorbis,h264,hevc,av1,vp8,vp9,mpeg4',
  '--enable-parser=h264,hevc,av1,vp9,vp8,aac,mpegaudio,opus,vorbis,mpeg4video',
  '--enable-bsf=aac_adtstoasc,extract_extradata,h264_mp4toannexb,hevc_mp4toannexb,av1_frame_merge,vp9_superframe',
];
if (windows) options.push('--target-os=mingw32', '--extra-ldflags=-static');
if (crossWindows) options.push('--enable-cross-compile', '--arch=x86_64', '--cross-prefix=x86_64-w64-mingw32-');
if (platform === 'macos') {
  options.push('--extra-cflags=-mmacosx-version-min=12.0', '--extra-ldflags=-mmacosx-version-min=12.0');
}

function verifyMerger(ffmpeg, cwd) {
  const actual = execFileSync(ffmpeg, ['-version'], { encoding: 'utf8' });
  if (!actual.startsWith(`ffmpeg version ${version} `)) throw new Error('Unexpected FFmpeg version');
  // Exercise the actual merger with separate tracks before bundling on every OS.
  const fixture = join(root, 'src-tauri', 'tests', 'fixtures');
  const run = args => execFileSync(ffmpeg, ['-v', 'error', '-nostdin', ...args], { cwd, stdio: 'inherit' });
  run(['-i', join(fixture, 'solid-1080p.mp4'), '-i', join(fixture, 'silence.ts'),
    '-map', '0:v:0', '-map', '1:a:0', '-c', 'copy', '-bsf:a', 'aac_adtstoasc', 'merged.mp4']);
  run(['-i', 'merged.mp4', '-map', '0:v:0', '-map', '0:a:0', '-c', 'copy', 'verified.mkv']);
}

const source = await mkdtemp(join(tmpdir(), 'versadown-ffmpeg-'));
const executable = windows ? 'ffmpeg.exe' : 'ffmpeg';
const target = join(root, 'src-tauri', 'binaries', `video-${platform}`, executable);
const staging = `${target}.${process.pid}.tmp`;
try {
  const expected = process.env.VD_WINDOWS_FFMPEG_SHA256;
  if (expected !== undefined) {
    if (process.platform !== 'win32' || !/^[0-9a-f]{64}$/.test(expected)
        || createHash('sha256').update(await readFile(target)).digest('hex') !== expected) {
      throw new Error('The Windows FFmpeg artifact did not match the producer job');
    }
    verifyMerger(target, source);
    console.log(`Verified Windows FFmpeg ${version} from the cross-build job`);
  } else {
    const archive = await sourceArchive('ffmpeg');
    execFileSync('tar', ['-xf', archive, '--strip-components', '1', '-C', source]);
    // Use MSYS make with the native UCRT64 compiler (not MSYS GCC or Git Bash).
    const msys = process.env.VD_MSYS2_ROOT || 'C:\\msys64';
    const shell = process.platform === 'win32' ? join(msys, 'usr', 'bin', 'bash.exe') : 'sh';
    const init = process.platform === 'win32' ? 'export PATH=/ucrt64/bin:/usr/bin:$PATH\n' : '';
    const jobs = Math.min(8, availableParallelism());
    execFileSync(shell, ['-ec', `${init}sh ./configure "$@"\nmake -j${jobs} ${executable}`, 'build-ffmpeg', ...options], {
      cwd: source, stdio: 'inherit',
      env: { ...process.env, ...(process.platform === 'win32' ? { MSYSTEM: 'UCRT64' } : {}) },
    });

    const ffmpeg = join(source, executable);
    // A Windows PE is validated by the consuming Windows runner before packaging.
    if (!crossWindows) verifyMerger(ffmpeg, source);
    await mkdir(dirname(target), { recursive: true });
    await copyFile(ffmpeg, staging);
    await chmod(staging, 0o755);
    await rename(staging, target);
    console.log(`Prepared FFmpeg ${version}: ${target}`);
  }
} finally {
  await rm(staging, { force: true });
  await rm(source, { recursive: true, force: true });
}
