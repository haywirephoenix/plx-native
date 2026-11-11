#!/usr/bin/env python3
"""Build the DEMO BUNDLE: tests/mock_pms.py's demo library, saved as files the app serves to itself.

    python3 tools/demo_bundle.py                       # writes pkg/demo/ (index.json + b/<hash>.<ext>)
    python3 tools/demo_bundle.py --out DIR             # somewhere else
    python3 tools/demo_bundle.py --requests rec.json   # also replay a recorded session's requests
    python3 tools/demo_bundle.py --selftest            # the normaliser and image-size reader, no mock

A `DEMO=1` build (`make DEMO=1 ipk`, the `demo build` workflow) packages pkg/demo/ and compiles the
`demo-pms` feature: at boot the app answers its own PMS requests on its compiled-in loopback
origin from this bundle (rust-modules/src/demo/), so a television runs the demo library with no
computer attached. docs/demo-pms.md is the design.

Where the answers come from: the mock itself, started here on a free loopback port with
`--catalog tests/demo_library/catalog.json`, so the bundle cannot drift from what `tools/tv-mock.sh
--demo` serves. Which requests are saved:

* a CRAWL — the section, hub and library roots, then every `key` a response names under
  `/library/` or `/hubs/` (breadth first, bounded), every item's detail sub-pages, every person,
  a play queue for every playable item and the part files those items name;
* every ARTWORK path a response names, fetched through `/photo/:/transcode` at several widths
  (the server returns the smallest stored size that covers the box the app asks for);
* optionally a RECORDING: `--requests FILE`, the JSON `GET /_mock/requests` returns after a
  `tools/tv-mock.sh --demo` session (`tv-mock.sh --demo --record FILE` saves it on quit), so a page
  the crawl misses but the app reads gets saved too. A miss on the television is logged by the app
  as `demo: miss GET <key>`.

Requests are matched by a KEY that drops what differs per device and per session. The same rules
are in rust-modules/src/demo/bundle.rs (`normalize`); `--selftest` and that module's tests pin the
same vectors, so the two cannot disagree silently:

* method upper-cased (HEAD is looked up as GET); path kept exactly as sent;
* query decoded, then every `X-Plex-*` parameter (identity, product, session, paging) and any
  parameter whose name contains `token` dropped, the rest sorted and re-encoded (unreserved
  characters `A-Za-z0-9-._~` kept, everything else `%XX`);
* `/photo/:/transcode` keeps only `url` and `format` (the size picks a stored variant instead);
* each entry is also filed under a LOOSE key (only `path`, `uri`, `url`, `key`, `query`, `type`
  kept), and under the bare path, which the server tries when the full key misses.

Paging is not part of the key: a saved listing is whole, and the server cuts the window the app
asks for out of it.
"""
import argparse
import hashlib
import json
import os
import pathlib
import shutil
import socket
import struct
import subprocess
import sys
import tempfile
import time
import urllib.error
import urllib.parse
import urllib.request

REPO = pathlib.Path(__file__).resolve().parent.parent
CATALOG = REPO / "tests" / "demo_library" / "catalog.json"
SYNTHETIC = "1.41.0.0000-synthetic"
FORMAT = 1

UNRESERVED = set("ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-._~")
LOOSE_PARAMS = ("path", "uri", "url", "key", "query", "type")
PHOTO = "/photo/:/transcode"
PHOTO_PARAMS = ("url", "format")
IMAGE_FIELDS = ("thumb", "art", "clearLogo", "parentThumb", "parentArt", "grandparentThumb",
                "grandparentArt", "composite", "banner", "theme")
IMAGE_WIDTHS = (240, 480, 960)
LANDSCAPE_WIDTHS = (1920,)
DETAIL_SUBPAGES = ("children", "extras", "similar", "allLeaves", "related")
CRAWL_LIMIT = 4000
# A media part bigger than this is left out (`--media-mb`): the package goes to a television whose app
# partition is ~615 MB in all. The demo library's one full film (Sintel, ~276 MB) is the case.
MEDIA_MB = 32


# ------------------------------------------------------------------------- the key ---
def enc(s):
    return "".join(c if c in UNRESERVED else "".join(f"%{b:02X}" for b in c.encode()) for c in s)


def parse_query(q):
    return urllib.parse.parse_qsl(q, keep_blank_values=True)


def kept(name):
    low = name.lower()
    return not low.startswith("x-plex-") and "token" not in low


def canonical(pairs):
    return "&".join(f"{enc(k)}={enc(v)}" for k, v in sorted(pairs))


def normalize(method, target):
    """(full key, loose key, bare key) for one request. Mirrors rust-modules/src/demo/bundle.rs."""
    method = method.upper()
    if method == "HEAD":
        method = "GET"
    path, _, query = target.partition("?")
    pairs = [(k, v) for k, v in parse_query(query) if kept(k)]
    if path == PHOTO:
        pairs = [(k, v) for k, v in pairs if k in PHOTO_PARAMS]
    full = canonical(pairs)
    loose = canonical([(k, v) for k, v in pairs if k in LOOSE_PARAMS])
    head = f"{method} {path}"
    return (f"{head}?{full}" if full else head,
            f"{head}?{loose}" if loose else head,
            head)


# ------------------------------------------------------------- image dimensions ---
def image_size(data):
    """(width, height) of a PNG or JPEG, or None."""
    if data[:8] == b"\x89PNG\r\n\x1a\n" and len(data) >= 24:
        return struct.unpack(">II", data[16:24])
    if data[:2] == b"\xff\xd8":
        i = 2
        while i + 9 < len(data):
            if data[i] != 0xFF:
                i += 1
                continue
            marker = data[i + 1]
            if marker in (0xD8, 0x01) or 0xD0 <= marker <= 0xD7:
                i += 2
                continue
            seg = struct.unpack(">H", data[i + 2:i + 4])[0]
            if 0xC0 <= marker <= 0xCF and marker not in (0xC4, 0xC8, 0xCC):
                h, w = struct.unpack(">HH", data[i + 5:i + 9])
                return w, h
            i += 2 + seg
    return None


# ------------------------------------------------------------------- the mock ---
def free_port():
    with socket.socket() as s:
        s.bind(("127.0.0.1", 0))
        return s.getsockname()[1]


class Mock:
    def __init__(self, cache=None):
        self.port = free_port()
        cmd = [sys.executable, "-X", "utf8", str(REPO / "tests" / "mock_pms.py"),
               "--port", str(self.port), "--catalog", str(CATALOG)]
        if cache:
            cmd += ["--catalog-cache", str(cache)]
        env = dict(os.environ, PYTHONUTF8="1")
        # The mock logs a line per write it receives. Into a file, never an unread pipe: a full pipe
        # blocks the mock mid-response, which reads as a request that never finishes.
        self.log = tempfile.TemporaryFile()
        self.proc = subprocess.Popen(cmd, stdout=self.log, stderr=subprocess.STDOUT, env=env)
        deadline = time.monotonic() + 60
        while time.monotonic() < deadline:
            if self.proc.poll() is not None:
                self.log.seek(0)
                raise SystemExit("demo_bundle: the mock exited: "
                                 + self.log.read().decode(errors="replace")[-2000:])
            try:
                status, _, body = self.fetch("GET", "/identity")
                if status == 200 and SYNTHETIC.encode() in body:
                    return
            except OSError:
                pass
            time.sleep(0.3)
        self.close()
        raise SystemExit("demo_bundle: the mock did not answer within 60 s")

    def fetch(self, method, target):
        req = urllib.request.Request(f"http://127.0.0.1:{self.port}{target}", method=method,
                                     headers={"Accept": "application/json", "X-Plex-Token": "demo"})
        opener = urllib.request.build_opener(urllib.request.ProxyHandler({}))
        try:
            with opener.open(req, timeout=60) as r:
                return r.status, r.headers.get("Content-Type", "application/octet-stream"), r.read()
        except urllib.error.HTTPError as e:
            return e.code, e.headers.get("Content-Type", "text/plain"), e.read()
        except TimeoutError:
            # A stream that never ends (an event feed) is not something a bundle can hold.
            print(f"demo_bundle: {method} {target} did not finish in 60 s; not saved", file=sys.stderr)
            return 504, "text/plain", b"timed out"

    def close(self):
        self.proc.terminate()
        try:
            self.proc.wait(10)
        except subprocess.TimeoutExpired:
            self.proc.kill()
        self.log.close()


# ------------------------------------------------------------------ the bundle ---
EXT = {"application/json": "json", "image/jpeg": "jpg", "image/png": "png", "video/mp4": "mp4",
       "video/x-matroska": "mkv", "text/plain": "txt", "application/xml": "xml", "text/xml": "xml"}


class Bundle:
    def __init__(self, out):
        self.out = pathlib.Path(out)
        self.entries = {}
        self.images = {}
        self.bytes = 0

    def _store(self, ctype, body):
        digest = hashlib.sha256(body).hexdigest()[:20]
        name = f"b/{digest}.{EXT.get(ctype.split(';')[0].strip(), 'bin')}"
        dest = self.out / name
        if not dest.exists():
            dest.parent.mkdir(parents=True, exist_ok=True)
            dest.write_bytes(body)
            self.bytes += len(body)
        return name

    def add(self, method, target, status, ctype, body):
        full, loose, bare = normalize(method, target)
        rec = {"status": status, "type": ctype.split(";")[0].strip(), "file": self._store(ctype, body)}
        self.entries[full] = rec
        # The loose and bare forms are fall-backs: the first answer filed under one keeps it.
        for k in (loose, bare):
            self.entries.setdefault(k, rec)

    def add_image(self, url, width, height, ctype, body):
        variants = self.images.setdefault(url, [])
        if any(v["w"] == width and v["h"] == height for v in variants):
            return
        variants.append({"w": width, "h": height, "type": ctype.split(";")[0].strip(),
                         "file": self._store(ctype, body)})
        variants.sort(key=lambda v: (v["w"] * v["h"], v["w"]))

    def write(self, identifier):
        index = {"format": FORMAT, "machineIdentifier": identifier,
                 "entries": dict(sorted(self.entries.items())),
                 "images": dict(sorted(self.images.items()))}
        (self.out / "index.json").write_text(json.dumps(index, indent=1, sort_keys=False) + "\n",
                                             encoding="utf-8")


# ------------------------------------------------------------------- the crawl ---
def walk_json(obj):
    if isinstance(obj, dict):
        yield obj
        for v in obj.values():
            yield from walk_json(v)
    elif isinstance(obj, list):
        for v in obj:
            yield from walk_json(v)


def crawl(mock, bundle, recorded, media_mb=MEDIA_MB):
    _, _, ident = mock.fetch("GET", "/identity")
    identifier = json.loads(ident)["MediaContainer"].get("machineIdentifier", "")
    queue, seen = [], set()

    def want(method, target):
        k = (method, target)
        if k not in seen:
            seen.add(k)
            queue.append(k)

    for t in ("/", "/identity", "/library/sections", "/hubs", "/hubs/continueWatching",
              "/hubs/continueWatching/items", "/hubs/promoted", "/library/all", "/library/onDeck",
              "/playlists/all", "/media/providers"):
        want("GET", t)
    for method, target in recorded:
        want(method, target)

    images, items, parts = set(), {}, set()
    misses = 0
    while queue:
        if len(seen) > CRAWL_LIMIT:
            raise SystemExit(f"demo_bundle: more than {CRAWL_LIMIT} requests — a link loop?")
        method, target = queue.pop(0)
        t0 = time.monotonic()
        status, ctype, body = mock.fetch(method, target)
        took = time.monotonic() - t0
        if target in parts and len(body) > media_mb * 1_000_000:
            print(f"demo_bundle: left out {target} ({len(body) / 1e6:.0f} MB > --media-mb {media_mb});"
                  " playing it answers 404", file=sys.stderr)
            continue
        if took > 5:
            print(f"demo_bundle: slow ({took:.0f} s): {method} {target}", file=sys.stderr)
        if len(seen) % 100 == 0:
            print(f"demo_bundle: {len(seen)} requests queued, {len(queue)} to go", file=sys.stderr)
        if status >= 400:
            misses += 1
        bundle.add(method, target, status, ctype, body)
        if "json" not in ctype or status != 200:
            continue
        try:
            doc = json.loads(body)
        except ValueError:
            continue
        for node in walk_json(doc):
            for f in IMAGE_FIELDS:
                v = node.get(f)
                if isinstance(v, str) and v.startswith("/library/"):
                    images.add(v)
            key = node.get("key")
            if isinstance(key, str) and key.startswith(("/library/", "/hubs/")) and "/parts/" not in key:
                want("GET", key)
            rk = node.get("ratingKey")
            if isinstance(rk, (str, int)) and str(rk).isdigit():
                items.setdefault(str(rk), node.get("type"))
            for media in node.get("Media", []) if isinstance(node.get("Media"), list) else []:
                for part in media.get("Part", []):
                    if isinstance(part.get("key"), str):
                        parts.add(part["key"])
            sec = node.get("librarySectionID")
            if sec is not None and str(sec).isdigit():
                for sub in ("all", "recentlyAdded", "collections"):
                    want("GET", f"/library/sections/{sec}/{sub}")
                want("GET", f"/hubs/sections/{sec}")
            for person in node.get("Role", []) if isinstance(node.get("Role"), list) else []:
                pid = person.get("id")
                if pid is not None:
                    want("GET", f"/library/people/{pid}")
                    want("GET", f"/library/people/{pid}/credits")
                    want("GET", f"/library/people/{pid}/media")
        if not queue:
            for rk, kind in sorted(items.items()):
                want("GET", f"/library/metadata/{rk}")
                for sub in DETAIL_SUBPAGES:
                    want("GET", f"/library/metadata/{rk}/{sub}")
                if kind in ("movie", "episode"):
                    uri = f"server://{identifier}/com.plexapp.plugins.library/library/metadata/{rk}"
                    q = urllib.parse.urlencode({"type": "video", "uri": uri, "continuous": "1",
                                                "shuffle": "0", "repeat": "0"})
                    want("POST", f"/playQueues?{q}")
            for p in sorted(parts):
                want("GET", p)

    # The queues the crawl created, so a GET of the id a canned POST answered finds its rows.
    for method, target in list(seen):
        if method == "POST" and target.startswith("/playQueues?"):
            full, _, _ = normalize(method, target)
            rec = bundle.entries.get(full)
            if rec:
                doc = json.loads((bundle.out / rec["file"]).read_bytes())
                qid = doc.get("MediaContainer", {}).get("playQueueID")
                if qid is not None:
                    status, ctype, body = mock.fetch("GET", f"/playQueues/{qid}")
                    bundle.add("GET", f"/playQueues/{qid}", status, ctype, body)

    for url in sorted(images):
        # The original is read for its shape only: the app asks through the photo transcoder.
        status, ctype, body = mock.fetch("GET", url)
        widths = IMAGE_WIDTHS
        size = image_size(body) if status == 200 else None
        if size and size[0] > size[1]:
            widths = IMAGE_WIDTHS + LANDSCAPE_WIDTHS
        for w in widths:
            q = urllib.parse.urlencode({"width": w, "height": 1, "minSize": 1, "url": url})
            status, ctype, body = mock.fetch("GET", f"{PHOTO}?{q}")
            dims = image_size(body) if status == 200 else None
            if dims:
                bundle.add_image(url, dims[0], dims[1], ctype, body)
    return identifier, len(seen), misses, len(images)


def load_recording(path):
    rows = json.loads(pathlib.Path(path).read_text(encoding="utf-8"))
    out = []
    for r in rows:
        method, path_ = r.get("method", "GET").upper(), r.get("path", "")
        if not path_.startswith("/") or path_.startswith("/_mock/"):
            continue
        # Writes other than a play queue change the mock's state and are acknowledged generically.
        if method not in ("GET", "HEAD") and not (method == "POST" and path_ == "/playQueues"):
            continue
        q = urllib.parse.urlencode(sorted((r.get("query") or {}).items()))
        out.append((method if method != "HEAD" else "GET", f"{path_}?{q}" if q else path_))
    return out


# ------------------------------------------------------------------ selftest ---
# The vectors rust-modules/src/demo/tests.rs pins too.
VECTORS = [
    ("GET", "/library/sections", ("GET /library/sections",) * 3),
    ("HEAD", "/identity?X-Plex-Token=abc", ("GET /identity",) * 3),
    ("GET", "/library/sections/1/all?type=1&sort=titleSort&X-Plex-Container-Start=0&X-Plex-Container-Size=50"
            "&X-Plex-Client-Identifier=c",
     ("GET /library/sections/1/all?sort=titleSort&type=1", "GET /library/sections/1/all?type=1",
      "GET /library/sections/1/all")),
    ("POST", "/playQueues?type=video&uri=server%3A%2F%2Fm%2Fcom.plexapp.plugins.library%2Flibrary%2Fmetadata%2F120"
             "&continuous=1&shuffle=0&repeat=0&X-Plex-Session-Identifier=s",
     ("POST /playQueues?continuous=1&repeat=0&shuffle=0&type=video"
      "&uri=server%3A%2F%2Fm%2Fcom.plexapp.plugins.library%2Flibrary%2Fmetadata%2F120",
      "POST /playQueues?type=video&uri=server%3A%2F%2Fm%2Fcom.plexapp.plugins.library%2Flibrary%2Fmetadata%2F120",
      "POST /playQueues")),
    ("GET", "/photo/:/transcode?width=250&height=375&minSize=1&url=%2Flibrary%2Fmetadata%2F42%2Fthumb%2F1",
     ("GET /photo/:/transcode?url=%2Flibrary%2Fmetadata%2F42%2Fthumb%2F1",
      "GET /photo/:/transcode?url=%2Flibrary%2Fmetadata%2F42%2Fthumb%2F1", "GET /photo/:/transcode")),
    ("GET", "/hubs/search?query=big+buck&limit=3&includeCollections=1&accessToken=x",
     ("GET /hubs/search?includeCollections=1&limit=3&query=big%20buck", "GET /hubs/search?query=big%20buck",
      "GET /hubs/search")),
]


def selftest():
    for method, target, want in VECTORS:
        got = normalize(method, target)
        assert got == want, (method, target, got, want)
    png = b"\x89PNG\r\n\x1a\n" + b"\x00\x00\x00\rIHDR" + struct.pack(">II", 640, 360) + b"\x08\x02\x00\x00\x00"
    assert image_size(png) == (640, 360)
    jpeg = b"\xff\xd8" + b"\xff\xe0\x00\x04xx" + b"\xff\xc0\x00\x11\x08" + struct.pack(">HH", 375, 250) + b"\x03"
    assert image_size(jpeg) == (250, 375), image_size(jpeg)
    print("demo_bundle selftest: ok")


def main():
    ap = argparse.ArgumentParser(description=__doc__.split("\n\n")[0])
    ap.add_argument("--out", type=pathlib.Path, default=REPO / "pkg" / "demo")
    ap.add_argument("--requests", type=pathlib.Path, action="append", default=[],
                    help="a saved GET /_mock/requests (repeatable)")
    ap.add_argument("--catalog-cache", type=pathlib.Path,
                    help="the demo library cache (default $PLXNATIVE_DEMO_CACHE or ~/.cache/plxnative-demo)")
    ap.add_argument("--media-mb", type=int, default=MEDIA_MB,
                    help=f"leave out media parts bigger than this (default {MEDIA_MB})")
    ap.add_argument("--selftest", action="store_true")
    args = ap.parse_args()
    if args.selftest:
        return selftest()
    recorded = [r for f in args.requests for r in load_recording(f)]
    tmp = args.out.with_name(args.out.name + ".new")
    shutil.rmtree(tmp, ignore_errors=True)
    tmp.mkdir(parents=True)
    bundle = Bundle(tmp)
    mock = Mock(args.catalog_cache)
    try:
        identifier, requests, misses, images = crawl(mock, bundle, recorded, args.media_mb)
    finally:
        mock.close()
    bundle.write(identifier)
    shutil.rmtree(args.out, ignore_errors=True)
    tmp.rename(args.out)
    print(f"demo_bundle: {requests} requests ({misses} answered 4xx/5xx), {images} images, "
          f"{len(bundle.entries)} keys, {bundle.bytes / 1e6:.1f} MB -> {args.out}")


if __name__ == "__main__":
    sys.exit(main())
