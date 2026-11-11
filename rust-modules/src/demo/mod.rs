//! **The demo build's own PMS** (`demo-pms` feature, `make DEMO=1`): the app answers its own Plex
//! requests from the demo bundle packaged beside it, so a television runs the demo library with no
//! computer attached. docs/demo-pms.md is the design; `tools/demo_bundle.py` writes the bundle from
//! `tests/mock_pms.py --catalog tests/demo_library/catalog.json`.
//!
//! Every byte the client reads from a PMS goes to the compiled-in origin over plaintext (the REST
//! door's plaintext arm, `ff.rs`'s media source and the artwork fetches all dial it), and a demo
//! build compiles in loopback. So the server binds that origin and NOTHING in the client changes:
//! the requests it answers are the real client's, through the real transports.
//!
//! [`start`] runs once, from `enter_application` before `boot` can send the first request, and on a
//! live boot only (a controlled replay reads recorded results and must not be answered by a
//! server). It reads the index and binds the socket on the calling thread, so [`active`] is true
//! exactly when the server is answering; the accept loop and each connection run on workers from
//! `plx_base::task`. A missing or unreadable bundle, a format this build does not read, a
//! non-loopback origin or a port in use leaves the build an ordinary debug build, with one log line
//! saying why. A request the bundle has no answer for is a 404 and one `demo: miss <key>` line (per
//! key), which is how a gap in the crawl shows up on a television.

pub(crate) mod bundle;
#[cfg(test)]
mod tests;

use std::collections::HashSet;
use std::ffi::{c_char, c_int, CStr};
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use bundle::{Answer, Index};

/// The identity a demo boot signs in with: the synthetic guest token `tools/mock-guest.py` gives a
/// `tools/tv-session.sh up --guest --mock` boot. The bundle answers any token; this one is a
/// fabricated value no real server accepts.
pub(crate) const TOKEN: &str = "plxnative-mock-guest";

/// Where the bundle sits, under the app directory.
const DIR: &str = "demo";
/// The longest request head read; a longer one is refused.
const HEAD_BYTES: usize = 16 * 1024;
/// A request body this server reads and drops (writes are acknowledged, never applied).
const BODY_BYTES: usize = 1024 * 1024;
/// How many distinct missed keys are logged before the server stops naming new ones.
const MISS_LINES: usize = 256;
const READ_WAIT: Duration = Duration::from_secs(15);
const WRITE_WAIT: Duration = Duration::from_secs(60);

static ACTIVE: AtomicBool = AtomicBool::new(false);

/// True once [`start`] has the bundle loaded and the socket bound: the boot then signs in with
/// [`TOKEN`] (`dev::scenarios::dev_token`).
pub(crate) fn active() -> bool {
    ACTIVE.load(Ordering::Acquire)
}

fn log(m: &str) {
    plx_base::eventlog::log(m);
}

struct Server {
    index: Index,
    dir: PathBuf,
    missed: Mutex<HashSet<String>>,
}

/// Load the bundle and bind `pms_host:pms_port`, then serve it on a worker. See the module doc.
pub(crate) fn start(pms_host: *const c_char, pms_port: c_int) {
    let host = if pms_host.is_null() {
        String::new()
    } else {
        // SAFETY: `src/main.c` passes its compiled-in string literal, which lives for the process.
        unsafe { CStr::from_ptr(pms_host) }.to_string_lossy().into_owned()
    };
    if host != "127.0.0.1" {
        log(&format!("demo: not started — the compiled-in PMS host is not loopback ({})",
            if host.is_empty() { "none" } else { "a LAN or remote address" }));
        return;
    }
    let Ok(port) = u16::try_from(pms_port) else {
        log("demo: not started — the compiled-in PMS port is out of range");
        return;
    };
    let dir = plx_base::paths::in_app_dir(DIR);
    let index = match std::fs::read(dir.join("index.json")) {
        Ok(bytes) => match serde_json::from_slice::<Index>(&bytes) {
            Ok(index) if index.format == bundle::FORMAT => index,
            Ok(index) => {
                log(&format!("demo: not started — bundle format {} (this build reads {})", index.format, bundle::FORMAT));
                return;
            }
            Err(e) => {
                log(&format!("demo: not started — index.json does not parse: {e}"));
                return;
            }
        },
        Err(e) => {
            log(&format!("demo: not started — no bundle at {}/index.json ({e})", dir.display()));
            return;
        }
    };
    let listener = match TcpListener::bind(("127.0.0.1", port)) {
        Ok(l) => l,
        Err(e) => {
            log(&format!("demo: not started — cannot bind 127.0.0.1:{port}: {e}"));
            return;
        }
    };
    let keys = index.entries.len();
    let images = index.images.len();
    let server = Arc::new(Server { index, dir, missed: Mutex::new(HashSet::new()) });
    if plx_base::task::spawn("demo-pms accept", move || accept_loop(&listener, &server)).is_none() {
        log("demo: not started — the accept worker was refused");
        return;
    }
    ACTIVE.store(true, Ordering::Release);
    log(&format!("demo: serving the demo bundle ({keys} keys, {images} images) on 127.0.0.1:{port}"));
}

fn accept_loop(listener: &TcpListener, server: &Arc<Server>) {
    for conn in listener.incoming() {
        let Ok(stream) = conn else { continue };
        let server = Arc::clone(server);
        // A refused worker drops the connection; the client sees a reset and retries as for any server.
        let _ = plx_base::task::spawn_small("demo-pms conn", move || serve(stream, &server));
    }
}

fn reason(status: u16) -> &'static str {
    match status {
        200 => "OK",
        206 => "Partial Content",
        400 => "Bad Request",
        404 => "Not Found",
        416 => "Range Not Satisfiable",
        500 => "Internal Server Error",
        _ => "Status",
    }
}

fn respond(stream: &mut TcpStream, status: u16, ctype: &str, body: &[u8], extra: &str, head_only: bool) {
    let header = format!(
        "HTTP/1.1 {status} {}\r\nContent-Type: {ctype}\r\nContent-Length: {}\r\nAccept-Ranges: bytes\r\n{extra}Connection: close\r\n\r\n",
        reason(status),
        body.len()
    );
    let _ = stream.write_all(header.as_bytes());
    if !head_only {
        let _ = stream.write_all(body);
    }
    let _ = stream.flush();
}

/// Read up to the end of the head; `None` for a closed, slow or oversized one.
fn read_head(stream: &mut TcpStream) -> Option<(bundle::Head, Vec<u8>)> {
    let mut buf = Vec::with_capacity(1024);
    let mut chunk = [0u8; 2048];
    loop {
        let n = stream.read(&mut chunk).ok()?;
        if n == 0 {
            return None;
        }
        buf.extend_from_slice(&chunk[..n]);
        if let Some(end) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
            let head = bundle::parse_head(&String::from_utf8_lossy(&buf[..end]))?;
            return Some((head, buf.split_off(end + 4)));
        }
        if buf.len() > HEAD_BYTES {
            return None;
        }
    }
}

fn serve(mut stream: TcpStream, server: &Server) {
    let _ = stream.set_read_timeout(Some(READ_WAIT));
    let _ = stream.set_write_timeout(Some(WRITE_WAIT));
    let Some((head, already)) = read_head(&mut stream) else {
        respond(&mut stream, 400, "text/plain", b"bad request", "", false);
        return;
    };
    // Drain a small body so the client's write completes; it is never applied.
    let mut left = head.content_length.min(BODY_BYTES).saturating_sub(already.len());
    let mut sink = [0u8; 4096];
    while left > 0 {
        match stream.read(&mut sink[..left.min(4096)]) {
            Ok(0) | Err(_) => break,
            Ok(n) => left -= n,
        }
    }
    let head_only = head.method.eq_ignore_ascii_case("HEAD");
    let (path, query) = head.target.split_once('?').unwrap_or((head.target.as_str(), ""));
    let (window, query) = plx_plex::plex::split_window(query);
    match server.index.answer(&head.method, path, &query) {
        Answer::Saved { status, ctype, file } => {
            let body = match std::fs::read(server.dir.join(file)) {
                Ok(b) => b,
                Err(e) => {
                    log(&format!("demo: bundle file {file} unreadable: {e}"));
                    respond(&mut stream, 500, "text/plain", b"bundle file unreadable", "", head_only);
                    return;
                }
            };
            if ctype == "application/json" {
                let body = window.and_then(|w| bundle::window_slice(&body, w)).unwrap_or(body);
                respond(&mut stream, status, ctype, &body, "", head_only);
                return;
            }
            match bundle::range(head.range.as_deref(), body.len() as u64) {
                Ok(Some((first, last))) => {
                    let extra = format!("Content-Range: bytes {first}-{last}/{}\r\n", body.len());
                    respond(&mut stream, 206, ctype, &body[first as usize..=last as usize], &extra, head_only);
                }
                Ok(None) => respond(&mut stream, status, ctype, &body, "", head_only),
                Err(()) => {
                    let extra = format!("Content-Range: bytes */{}\r\n", body.len());
                    respond(&mut stream, 416, "text/plain", b"", &extra, head_only);
                }
            }
        }
        Answer::Ack => respond(&mut stream, 200, "application/json", br#"{"MediaContainer":{"size":0}}"#, "", head_only),
        Answer::Miss(key) => {
            let first = server.missed.lock().is_ok_and(|mut seen| seen.len() < MISS_LINES && seen.insert(key.clone()));
            if first {
                log(&format!("demo: miss {key}"));
            }
            respond(&mut stream, 404, "text/plain", b"not in the demo bundle", "", head_only);
        }
    }
}
