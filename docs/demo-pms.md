# The demo build's own PMS (`DEMO=1`)

A `DEMO=1` package runs the demo library on a television with **no computer attached**: the app
answers its own Plex requests from a bundle packaged beside it. It is a developer build (debug
flavour, dev triggers on) and never a release.

```bash
python3 tools/demo_library.py derive     # once: the demo library's artwork (~390 MB of sources, cached)
python3 tools/demo_bundle.py             # writes pkg/demo/ (index.json + b/<hash>.<ext>)
make DEMO=1 ipk                          # packages pkg/demo/ with a `demo-pms` binary
```

The `demo build` workflow (`.github/workflows/demo-build.yml`) does all three and uploads the
package as `plxnative-demo-ipk`; that is the route for a machine that cannot cross-build.

## How it works

Every byte the client reads from a PMS goes to the compiled-in origin (`PMS_HOST:PMS_PORT`) over
plaintext: the REST door's plaintext arm (`plx_plex::http` → `plx_net::stream`), the media source
`ff.rs` opens for an `http://` part, and the artwork fetches. A demo build compiles in loopback
(`127.0.0.1:32400`), so `rust-modules/src/demo/` binds that origin and **nothing in the client
changes**: what it answers are the real client's requests, through the real transports.

* `demo::start` runs once from `app::enter_application`, before `boot` sends the first request, on a
  live boot only (a controlled replay reads recorded results). It reads `<app dir>/demo/index.json`
  and binds the socket on the boot thread, then accepts on a `plx_base::task` worker, one small
  worker per connection. HTTP/1.1, `Connection: close`, GET/HEAD/PUT/POST, byte ranges.
* `dev::scenarios::dev_token` falls back to the synthetic guest token (`plxnative-mock-guest`, the
  value `tools/mock-guest.py` gives a `--guest --mock` boot) when the server is answering and no
  `plxnative-token` trigger exists, so the boot signs in unattended. A trigger still wins.
* If the bundle is missing or unreadable, its `format` is not the one this build reads, the
  compiled-in host is not `127.0.0.1` or the port is taken, the server does not start, the app is an
  ordinary debug build, and the event log says why in one `demo: not started — …` line.

### Matching a request to a saved answer

`tools/demo_bundle.py` and `rust-modules/src/demo/bundle.rs` share one set of rules, and pin the
same vectors (`demo_bundle.py --selftest`, `demo/tests.rs`):

* the method upper-cased, HEAD looked up as GET; the path exactly as sent;
* the query decoded, every `X-Plex-*` parameter (identity, product, session, paging) and any
  parameter whose name contains `token` dropped, the rest sorted and re-encoded;
* each answer filed under that FULL key, a LOOSE key (only `path`, `uri`, `url`, `key`, `query`,
  `type` kept) and the bare path; the server tries them in that order;
* `/photo/:/transcode` is answered from the image's stored sizes by its `url`: the smallest stored
  size that covers the requested box, else the largest (every image is stored at widths 240, 480 and
  960, and a landscape one at 1920 too);
* a listing is saved whole and the server cuts the window the request asks for
  (`plx_plex::plex::split_window` reads it, the one paging owner), setting `offset`, `size` and
  `totalSize` as PMS does;
* a read with no saved answer is a 404 and one `demo: miss GET <key>` line per key (the first 256);
  a write with none (timeline, scrobble, `PUT /library/parts`, …) gets `200 {"MediaContainer":{"size":0}}`.

### What is in the bundle

`demo_bundle.py` starts `tests/mock_pms.py --catalog tests/demo_library/catalog.json` on a free
loopback port, so the bundle cannot drift from what `tools/tv-mock.sh --demo` serves, and saves:
the section, hub and library roots; every `key` a response names under `/library/` or `/hubs/`
(breadth first); every item's detail sub-pages (`children`, `extras`, `similar`, `allLeaves`,
`related`); every person; a play queue for every movie and episode (and its `GET /playQueues/<id>`);
the media parts items name; every artwork path at the sizes above. About 870 requests and 46 MB.

A media part over `--media-mb` (default 32) is left out and the generator says so: the demo
library's one full film, Sintel (~280 MB), plays against the computer's mock but answers 404 from the
bundle. The stand-in episode (Caminandes S1E2) plays from either.

**Coverage from a real session.** `tools/tv-mock.sh --demo --record FILE` saves every request the app
made (`GET /_mock/requests`) when it quits; `tools/demo_bundle.py --requests FILE` replays them
against the mock and saves those answers too. A `demo: miss` line on the television names a key to
add that way, or a crawl rule to add here.

## Limits

* Nothing is remembered: watched state, progress, the Watchlist and track picks are acknowledged and
  dropped (the next read answers what was saved).
* Search answers only the queries the crawl or a recording saved; other queries are 404s.
* No plex.tv: no QR sign-in, no account features; the boot is the dev-token guest boot.
* An app whose port is already held (a `tools/tv-mock.sh` tunnel opened before the launch) talks to
  that mock instead; the log says the demo server could not bind.
