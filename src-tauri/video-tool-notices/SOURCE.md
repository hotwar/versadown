# Web video tools and corresponding source

VersaDown invokes the following tools as separate command-line programs.

Download `VersaDown-video-tool-sources.tar.gz` alongside the installers for the upstream source archives, packaging scripts and license notices. For CI test builds, download the `video-tool-sources` artifact from the same GitHub Actions run.

## yt-dlp 2026.10.26

VersaDown packages the official PyInstaller build and applies platform signing during packaging where required. Its combined license is GPL-3.0-or-later. The yt-dlp source itself is released under the Unlicense. This directory includes the upstream [license](yt-dlp-LICENSE.txt) and [component notices and licenses](yt-dlp-THIRD_PARTY_LICENSES.txt).

- [Official release, source tarball and executable checksums](https://github.com/yt-dlp/yt-dlp/releases/tag/2026.10.26)
- [Tagged source and build scripts (ZIP)](https://github.com/yt-dlp/yt-dlp/archive/refs/tags/2026.10.26.zip)
- [Platform-specific build instructions](https://github.com/yt-dlp/yt-dlp/blob/2026.10.26/.github/workflows/build.yml) and [pinned component versions](https://github.com/yt-dlp/yt-dlp/blob/2026.10.26/pyproject.toml)

For each bundled dependency, the component notices give its upstream project. The pinned Python packages have source distributions on [PyPI](https://pypi.org), including [mutagen 1.48.1](https://pypi.org/project/mutagen/1.48.1/#files) and [yt-dlp-ejs 0.8.0](https://pypi.org/project/yt-dlp-ejs/0.8.0/#files). The notices also identify the native libraries and their upstream source locations.

## FFmpeg 8.1.3

VersaDown builds FFmpeg from the [8.1.3 release source](https://ffmpeg.org/releases/ffmpeg-8.1.3.tar.xz) for merging audio and video. This build uses FFmpeg's own libraries and is licensed under the GNU Lesser General Public License, version 2.1 or later. See [FFmpeg's license overview](FFmpeg-LICENSE.txt) and the [LGPL 2.1 text](FFmpeg-LGPL-2.1.txt).

The source download accompanying the installers contains `ffmpeg-8.1.3.tar.xz` (SHA-256 `7138d28c96d9d3e3af4ee3d8cad72741f8ffb40da90c1112235dea3ecd3178a3`) and the build recipe `prepare-video-ffmpeg.mjs`. In the VersaDown source tree, that recipe is under `scripts/`. It specifies the configure flags, enabled components and target-specific build commands.

## QuickJS-NG 0.18.0

The `qjs` executable supplies a JavaScript runtime for yt-dlp. The macOS build uses the [v0.18.0 source archive](https://github.com/quickjs-ng/quickjs/archive/refs/tags/v0.18.0.tar.gz), the upstream CMake scripts and the system allocator. See the [MIT license](QuickJS-NG-LICENSE.txt) and the [notices for C atomics and Unicode data](QuickJS-NG-component-notices.txt).

Windows and Linux ship the upstream [x86_64 release executables](https://github.com/quickjs-ng/quickjs/releases/tag/v0.18.0). Both include mimalloc under its [MIT license](mimalloc-LICENSE.txt). The [v0.18.0 release workflow](https://github.com/quickjs-ng/quickjs/blob/v0.18.0/.github/workflows/release.yml) identifies the MSYS2 and Alpine build environments.

- The Windows executable includes [mimalloc 3.5.3](https://github.com/microsoft/mimalloc/tree/v3.5.3) and MinGW-w64 runtime code. See the [MinGW-w64 notice](mingw-w64-COPYING.txt).
- The Linux executable includes [mimalloc 3.5.0](https://github.com/microsoft/mimalloc/tree/v3.5.0) and [musl 1.2.6](https://musl.libc.org/releases/musl-1.2.6.tar.gz). See the [musl copyright and license notices](musl-COPYRIGHT.txt).
