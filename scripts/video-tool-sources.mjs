import { join } from 'node:path';
import { fileURLToPath } from 'node:url';
import { preparePinnedAsset, version as ytVersion } from './prepare-video-extractor.mjs';

export const sourceDirectory = fileURLToPath(new URL('../.tools/video-sources/', import.meta.url));
export const ffmpegVersion = '8.1.3';
const ffmpegName = `ffmpeg-${ffmpegVersion}.tar.xz`;
const archives = {
  ffmpeg: {
    name: ffmpegName, url: `https://ffmpeg.org/releases/${ffmpegName}`,
    mirrors: [`https://distfiles.macports.org/ffmpeg/${ffmpegName}`],
    size: 11732036, sha256: '7138d28c96d9d3e3af4ee3d8cad72741f8ffb40da90c1112235dea3ecd3178a3',
  },
  quickjs: {
    name: 'quickjs-ng-0.18.0.tar.gz', url: 'https://codeload.github.com/quickjs-ng/quickjs/tar.gz/refs/tags/v0.18.0',
    size: 910210, sha256: '28bcd89aa3683fd2a946537e5ce3f11cc46b5cbf3383c7561aaef3df5ff9f1dd',
  },
  'yt-dlp': {
    name: `yt-dlp-${ytVersion}.tar.gz`, url: `https://github.com/yt-dlp/yt-dlp/releases/download/${ytVersion}/yt-dlp.tar.gz`,
    size: 6019106, sha256: '21f942d258d25cc4e60b4982a25c423460f0bebb835f38e6dfd1fcb140087f95',
  },
};

export function sourceDigest(component) {
  return archives[component].sha256;
}

export function sourceArchive(component) {
  const asset = archives[component];
  return preparePinnedAsset(join(sourceDirectory, asset.name), asset);
}
