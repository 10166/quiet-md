"""The board's Xiaolai font is fetched the first time a board needs it
(src/board/xiaolai.rs, docs/UPDATES.md).

Build with --release --features devtools, then run this file. FDL_BIN selects
another build. A local HTTP server stands in for download.quiet.md
(QUIET_MD_FONT_URL, which also makes the build ignore its compiled-in copy);
the font cache is a temporary folder (QUIET_MD_FONT_DIR). Screenshots land in
the work folder printed at the end (or BOARD_FONT_OUT).

1. Online: a Chinese text on a new board first draws with the system font,
   then, once the (deliberately slow) server has answered, with Xiaolai; the
   file is cached; a copy as SVG embeds Xiaolai.
2. Cached: a second launch uses the cache without asking the server.
3. Offline (no server, no cache): the board draws with the system font, the
   app keeps running, and a copy as SVG goes ahead without Xiaolai, with a
   toast saying so (offline-3-copied).
"""
import http.server
import os
from pathlib import Path
import re
import shutil
import subprocess
import sys
import tempfile
import threading
import time

here = Path(__file__).resolve().parent
repo = here.parent.parent
font = repo / "assets/fonts/excalidraw/Xiaolai-Regular.ttf.zlib"
name = re.search(r'NAME: &str = "([^"]+)"', (repo / "src/board/xiaolai.rs").read_text()).group(1)
work = Path(os.environ.get("BOARD_FONT_OUT") or tempfile.mkdtemp(prefix="quiet-board-font-"))
work.mkdir(parents=True, exist_ok=True)
serve = work / "serve"
serve.mkdir(exist_ok=True)
shutil.copy(font, serve / name)
hits = []
DELAY = 4
FACE = "@font-face { font-family: Xiaolai;"


class Host(http.server.SimpleHTTPRequestHandler):
    def __init__(self, *a, **k):
        super().__init__(*a, directory=str(serve), **k)

    def do_GET(self):
        hits.append(self.path)
        time.sleep(DELAY)  # long enough to see the fallback first
        super().do_GET()

    def log_message(self, *a):
        pass


def script(tag, wait_after):
    # 侧栏「+」› 画板, then a text (IME commit: Chinese), deselected
    return f"""move 700 700
wait 1500
click 251 106
wait 300
click 295 208
wait 1200
key t
click 650 380
commit Hello你好，世界！白板手写
wait 200
key escape
move 1100 700
wait 500
shot {tag}-0-start
wait {wait_after}
shot {tag}-1-later
clip clear
key cmd-a
rclick 700 390
wait 400
shot {tag}-2-menu
"""


def run(tag, url, cache, extra=""):
    out = work / tag
    out.mkdir(exist_ok=True)
    s = out / "script.txt"
    s.write_text(script(tag, 7000) + extra)
    env = {**os.environ, "QUIET_MD_FONT_URL": url, "QUIET_MD_FONT_DIR": str(cache)}
    subprocess.run([sys.executable, str(here / "run.py"), str(s), str(here / "lib_clean"), str(out)], env=env, check=True)
    return out


def main():
    cache = work / "cache"
    shutil.rmtree(cache, ignore_errors=True)
    server = http.server.ThreadingHTTPServer(("127.0.0.1", 0), Host)
    threading.Thread(target=server.serve_forever, daemon=True).start()
    url = f"http://127.0.0.1:{server.server_address[1]}/"
    # the menu's "Copy to clipboard as SVG" row (see the -2-menu shot)
    copy = f"click {MENU_SVG[0]} {MENU_SVG[1]}\nwait 1200\nshot {{}}-3-copied\nwait 1500\nclip save {{}}\n"

    online = run("online", url, cache, copy.format("online", work / "online" / "copy.svg"))
    assert hits == [f"/{name}"], hits
    assert (cache / name).read_bytes() == font.read_bytes(), "cached as served"
    svg = (online / "copy.svg").read_text()
    assert FACE in svg, "Xiaolai embedded"

    hits.clear()
    run("cached", url, cache)
    assert hits == [], f"the cache answers: {hits}"
    server.shutdown()
    server.server_close()  # connections are refused from now on

    shutil.rmtree(cache)
    offline = run("offline", url, cache, copy.format("offline", work / "offline" / "copy.svg"))
    assert not (cache / name).exists()
    svg = (offline / "copy.svg").read_text()
    assert svg.startswith("<svg") and FACE not in svg and "@font-face { font-family: Excalifont;" in svg, "the copy goes ahead without Xiaolai"
    for tag in ("online", "cached", "offline"):
        assert "panicked" not in (work / tag / "log").read_text(errors="replace"), f"{tag}: panic"
    print(f"board font: ok ({work})")


MENU_SVG = (790, 331)

if __name__ == "__main__":
    main()
