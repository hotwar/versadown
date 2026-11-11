import { execFileSync } from 'node:child_process';
import { mkdir } from 'node:fs/promises';
import { basename, join } from 'node:path';
import { fileURLToPath } from 'node:url';
import { sourceArchive, sourceDirectory } from './video-tool-sources.mjs';

const root = fileURLToPath(new URL('../', import.meta.url));
const archives = await Promise.all(['ffmpeg', 'quickjs', 'yt-dlp'].map(sourceArchive));
const output = join(root, 'artifacts', 'video-sources');
await mkdir(output, { recursive: true });
execFileSync('tar', ['-czf', join(output, 'VersaDown-video-tool-sources.tar.gz'),
  '-C', root, 'scripts/video-tool-sources.mjs', 'scripts/prepare-video-extractor.mjs', 'scripts/prepare-video-ffmpeg.mjs',
  'scripts/prepare-video-js.mjs', 'scripts/prepare-video-tools.mjs', 'src-tauri/video-tool-notices',
  'src-tauri/tests/fixtures/solid-1080p.mp4', 'src-tauri/tests/fixtures/silence.ts',
  '-C', sourceDirectory, ...archives.map(path => basename(path))], { stdio: 'inherit' });
