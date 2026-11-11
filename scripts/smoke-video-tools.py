"""Check offline site matching and merge a local fixture with installer tools."""
import functools
from http.server import SimpleHTTPRequestHandler, ThreadingHTTPServer
import json
import os
from pathlib import Path
import re
import subprocess
import sys
import tempfile
import threading


def check_site_matching(tools):
    # This checks the installed extractor's URL contract, not live site availability.
    fixtures = Path(__file__).resolve().parents[1] / "src-tauri/tests/fixtures"
    cases = json.loads((fixtures / "video-site-urls.json").read_text(encoding="utf-8"))
    executable = Path(tools).resolve() / ("yt-dlp.exe" if os.name == "nt" else "yt-dlp")
    result = subprocess.run(
        [str(executable), "--ignore-config", "--no-plugin-dirs", "--no-cache-dir",
         "--encoding", "utf-8", "--list-extractors", "--batch-file", "-"],
        input="\n".join(case["url"] for case in cases) + "\n",
        env=dict(os.environ, PATH=""), capture_output=True, text=True, encoding="utf-8", timeout=60)
    if result.returncode:
        raise RuntimeError(result.stderr)
    diagnostic = {"generic", "generic:quoted-html", "DRM", "Piracy", "Liability",
                  "CommonMistakes", "UnicodeBOM", "youtube:truncated_id", "youtube:truncated_url"}
    matches = {case["url"]: set() for case in cases}
    seen = set()
    extractor = None
    for line in result.stdout.splitlines():
        if line.startswith("  "):
            url = line[2:]
            seen.add(url)
            if extractor:
                matches[url].add(extractor)
        else:
            extractor = line if line and line not in diagnostic and not line.endswith(" (CURRENTLY BROKEN)") else None
    if seen != set(matches):
        raise AssertionError("yt-dlp did not report every site-matching input")
    for case in cases:
        expected = {case["extractor"]} if case["extractor"] else set()
        if matches[case["url"]] != expected:
            raise AssertionError(f"{case['site']}: expected {expected}, got {matches[case['url']]}")
    print(f"Bundled yt-dlp passed {len(cases)} offline site-matching cases; no live availability claim.")


def check(tools):
    def tool(name):
        path = tools / (name + (".exe" if os.name == "nt" else ""))
        if not path.is_file():
            raise FileNotFoundError(path)
        return str(path)

    check_site_matching(tools)
    # Hide runner-installed FFmpeg, JS runtimes and Python from the child processes.
    environment = dict(os.environ, PATH="", NO_PROXY="127.0.0.1", no_proxy="127.0.0.1")
    fixtures = Path(__file__).resolve().parents[1] / "src-tauri/tests/fixtures"
    with tempfile.TemporaryDirectory() as temporary:
        root = Path(temporary)
        (root / "video.mp4").write_bytes((fixtures / "solid-1080p.mp4").read_bytes())
        (root / "audio.m4a").write_bytes((fixtures / "silence-init.mp4").read_bytes()
                                        + (fixtures / "silence-segment.m4s").read_bytes())
        (root / "test.mpd").write_text("""<MPD xmlns="urn:mpeg:dash:schema:mpd:2011"
  type="static" mediaPresentationDuration="PT1S" minBufferTime="PT1S">
  <Period>
    <AdaptationSet mimeType="video/mp4" contentType="video">
      <Representation id="v" bandwidth="16000" codecs="avc1.640028" width="1920" height="1080">
        <BaseURL>video.mp4</BaseURL></Representation></AdaptationSet>
    <AdaptationSet mimeType="audio/mp4" contentType="audio">
      <Representation id="a" bandwidth="16000" codecs="mp4a.40.2">
        <BaseURL>audio.m4a</BaseURL></Representation></AdaptationSet>
  </Period></MPD>""", encoding="utf-8")
        handler = functools.partial(SimpleHTTPRequestHandler, directory=temporary)
        with ThreadingHTTPServer(("127.0.0.1", 0), handler) as server:
            threading.Thread(target=server.serve_forever, daemon=True).start()
            try:
                output = root / "merged.mp4"
                command = [tool("yt-dlp"), "--ignore-config", "--no-plugin-dirs", "--no-cache-dir",
                           "--verbose", "--no-js-runtimes", "--js-runtimes", "quickjs:" + tool("qjs"),
                           "--ffmpeg-location", tool("ffmpeg"), "--downloader", "m3u8,dash:native",
                           "--proxy", "", "--no-playlist",
                           "-f", "bv[height=1080]+ba", "--merge-output-format", "mp4", "-o", str(output),
                           f"http://127.0.0.1:{server.server_port}/test.mpd"]
                result = subprocess.run(command, cwd=root, env=environment, capture_output=True,
                                        text=True, timeout=100)
                if result.returncode or not re.search(r"JS runtimes:.*quickjs", result.stderr, re.I):
                    raise RuntimeError(result.stderr)
                streams = subprocess.run([tool("ffmpeg"), "-hide_banner", "-i", str(output)],
                                         env=environment, capture_output=True, text=True, timeout=20).stderr
                if not (re.search(r"Video: h264[^\n]*1920x1080", streams) and "Audio: aac" in streams):
                    raise RuntimeError(streams)
                print("Bundled yt-dlp recognized QuickJS and merged H.264 1920x1080 + AAC with bundled FFmpeg.")
            finally:
                server.shutdown()


if __name__ == "__main__":
    check(Path(sys.argv[1]).resolve())
