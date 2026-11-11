//! The demo bundle's index and every decision about a request that needs no socket: the key a
//! request is filed under, which stored image answers a size, the page window cut out of a whole
//! listing, a `Range` header and the request head itself. `tools/demo_bundle.py` writes the index;
//! its `normalize` and this one follow the same rules, and both pin the same vectors
//! (`tests.rs`, `demo_bundle.py --selftest`).

use std::collections::HashMap;

use plx_plex::plex::PageReq;
use serde::Deserialize;

/// The index format this build reads. `demo_bundle.py` writes the same number.
pub(crate) const FORMAT: u32 = 1;

/// The photo transcoder: keyed by its `url` alone, the size choosing among stored variants.
pub(crate) const PHOTO: &str = "/photo/:/transcode";
const PHOTO_PARAMS: [&str; 2] = ["url", "format"];
/// What a LOOSE key keeps: the parameters that say WHICH thing is asked for, not how.
const LOOSE_PARAMS: [&str; 6] = ["path", "uri", "url", "key", "query", "type"];
/// The listing arrays a window may be cut from, in the order a container is searched.
const LISTS: [&str; 5] = ["Metadata", "Directory", "Hub", "SearchResult", "Playlist"];

/// One saved answer: its status, its content type and the body file under the bundle directory.
#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
pub(crate) struct Entry {
    pub status: u16,
    #[serde(rename = "type")]
    pub ctype: String,
    pub file: String,
}

/// One stored size of an artwork image.
#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
pub(crate) struct Variant {
    pub w: u32,
    pub h: u32,
    #[serde(rename = "type")]
    pub ctype: String,
    pub file: String,
}

/// `index.json`.
#[derive(Debug, Default, Deserialize)]
pub(crate) struct Index {
    pub format: u32,
    #[serde(default)]
    pub entries: HashMap<String, Entry>,
    #[serde(default)]
    pub images: HashMap<String, Vec<Variant>>,
}

/// What the server does with one request.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Answer<'a> {
    /// Serve this saved body.
    Saved { status: u16, ctype: &'a str, file: &'a str },
    /// A write nobody saved an answer for: acknowledged with an empty container.
    Ack,
    /// A read nobody saved: 404, and the key is logged.
    Miss(String),
}

/// The three keys a request is looked up under, most specific first.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct Keys {
    pub full: String,
    pub loose: String,
    pub bare: String,
}

fn unreserved(b: u8) -> bool {
    b.is_ascii_alphanumeric() || matches!(b, b'-' | b'.' | b'_' | b'~')
}

fn enc(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        if unreserved(b) {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

fn hex(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

/// Form decoding, as Python's `parse_qsl`: `+` is a space, `%XX` a byte, a bad escape is literal.
fn decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        match b[i] {
            b'+' => out.push(b' '),
            b'%' if i + 2 < b.len() => {
                match (hex(b[i + 1]), hex(b[i + 2])) {
                    (Some(h), Some(l)) => {
                        out.push(h << 4 | l);
                        i += 3;
                        continue;
                    }
                    _ => out.push(b'%'),
                }
            }
            c => out.push(c),
        }
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn kept(name: &str) -> bool {
    let low = name.to_ascii_lowercase();
    !low.starts_with("x-plex-") && !low.contains("token")
}

fn canonical<'a>(pairs: impl Iterator<Item = &'a (String, String)>) -> String {
    let mut v: Vec<&(String, String)> = pairs.collect();
    v.sort();
    v.iter().map(|(k, val)| format!("{}={}", enc(k), enc(val))).collect::<Vec<_>>().join("&")
}

/// The decoded `(name, value)` pairs of a query string, in order.
pub(crate) fn pairs(query: &str) -> Vec<(String, String)> {
    query
        .split('&')
        .filter(|p| !p.is_empty())
        .map(|p| {
            let (k, v) = p.split_once('=').unwrap_or((p, ""));
            (decode(k), decode(v))
        })
        .collect()
}

/// The keys of `method target` (`target` = path, `?`, query). HEAD is looked up as GET; the path is
/// kept as sent; every `X-Plex-*` and token-named parameter is dropped and the rest sorted.
pub(crate) fn normalize(method: &str, target: &str) -> Keys {
    let mut method = method.to_ascii_uppercase();
    if method == "HEAD" {
        method = "GET".to_owned();
    }
    let (path, query) = target.split_once('?').unwrap_or((target, ""));
    let mut kept_pairs: Vec<(String, String)> = pairs(query).into_iter().filter(|(k, _)| kept(k)).collect();
    if path == PHOTO {
        kept_pairs.retain(|(k, _)| PHOTO_PARAMS.contains(&k.as_str()));
    }
    let full = canonical(kept_pairs.iter());
    let loose = canonical(kept_pairs.iter().filter(|(k, _)| LOOSE_PARAMS.contains(&k.as_str())));
    let head = format!("{method} {path}");
    let with = |q: &str| if q.is_empty() { head.clone() } else { format!("{head}?{q}") };
    Keys { full: with(&full), loose: with(&loose), bare: head.clone() }
}

/// The stored variant for a `w`×`h` box: the smallest that covers it, else the largest there is.
pub(crate) fn pick(variants: &[Variant], w: u32, h: u32) -> Option<&Variant> {
    let area = |v: &&Variant| u64::from(v.w) * u64::from(v.h);
    variants
        .iter()
        .filter(|v| v.w >= w && v.h >= h)
        .min_by_key(area)
        .or_else(|| variants.iter().max_by_key(area))
}

impl Index {
    /// The answer for `method path?query`, `query` already stripped of its page window.
    pub(crate) fn answer(&self, method: &str, path: &str, query: &str) -> Answer<'_> {
        if path == PHOTO {
            let q = pairs(query);
            let get = |name: &str| q.iter().find(|(k, _)| k == name).map(|(_, v)| v.as_str());
            if let Some(variants) = get("url").and_then(|url| self.images.get(url)) {
                let dim = |name: &str| get(name).and_then(|v| v.parse::<u32>().ok()).unwrap_or(0);
                if let Some(v) = pick(variants, dim("width"), dim("height")) {
                    return Answer::Saved { status: 200, ctype: &v.ctype, file: &v.file };
                }
            }
        }
        let target = if query.is_empty() { path.to_owned() } else { format!("{path}?{query}") };
        let keys = normalize(method, &target);
        let hit = self.entries.get(&keys.full).or_else(|| self.entries.get(&keys.loose))
            .or_else(|| self.entries.get(&keys.bare));
        match hit {
            Some(e) => Answer::Saved { status: e.status, ctype: &e.ctype, file: &e.file },
            None if keys.bare.starts_with("GET ") => Answer::Miss(keys.full),
            None => Answer::Ack,
        }
    }
}

/// A whole listing cut to the window `req` asks for, as PMS pages one: the first listing array the
/// container holds is sliced, `offset`/`size` describe the slice and `totalSize` keeps the whole
/// count. `None` when the body is not a JSON container with such an array (serve it as it is).
pub(crate) fn window_slice(body: &[u8], req: PageReq) -> Option<Vec<u8>> {
    let mut doc: serde_json::Value = serde_json::from_slice(body).ok()?;
    let mc = doc.get_mut("MediaContainer")?.as_object_mut()?;
    let name = LISTS.iter().find(|n| mc.get(**n).is_some_and(serde_json::Value::is_array))?;
    let list = mc.get_mut(*name)?.as_array_mut()?;
    let total = list.len();
    let start = req.start.min(total);
    let end = start.saturating_add(req.size).min(total);
    let page: Vec<serde_json::Value> = list.drain(start..end).collect();
    let n = page.len();
    *list = page;
    if !mc.contains_key("totalSize") {
        mc.insert("totalSize".into(), total.into());
    }
    mc.insert("offset".into(), start.into());
    mc.insert("size".into(), n.into());
    serde_json::to_vec(&doc).ok()
}

/// A `Range` header against a body of `len` bytes: `Ok(Some((first, last)))` inclusive, `Ok(None)`
/// to serve the whole body (no range, or one this server does not take: several ranges, a
/// non-byte unit), `Err(())` for a range wholly past the end (416).
pub(crate) fn range(header: Option<&str>, len: u64) -> Result<Option<(u64, u64)>, ()> {
    let Some(spec) = header.and_then(|h| h.trim().strip_prefix("bytes=")) else { return Ok(None) };
    if spec.contains(',') || len == 0 {
        return Ok(None);
    }
    let (a, b) = spec.split_once('-').ok_or(())?;
    let (a, b) = (a.trim(), b.trim());
    let (first, last) = if a.is_empty() {
        let n: u64 = b.parse().map_err(|_| ())?;
        if n == 0 {
            return Err(());
        }
        (len.saturating_sub(n), len - 1)
    } else {
        let first: u64 = a.parse().map_err(|_| ())?;
        let last = if b.is_empty() { len - 1 } else { b.parse::<u64>().map_err(|_| ())?.min(len - 1) };
        (first, last)
    };
    if first >= len || first > last {
        return Err(());
    }
    Ok(Some((first, last)))
}

/// A parsed request head.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct Head {
    pub method: String,
    pub target: String,
    pub range: Option<String>,
    pub content_length: usize,
}

/// The request line and the two headers this server reads, from the bytes before `\r\n\r\n`.
pub(crate) fn parse_head(text: &str) -> Option<Head> {
    let mut lines = text.split("\r\n");
    let mut first = lines.next()?.split(' ');
    let method = first.next()?.to_owned();
    let target = first.next()?.to_owned();
    if method.is_empty() || !target.starts_with('/') {
        return None;
    }
    let (mut range, mut content_length) = (None, 0);
    for line in lines {
        let Some((name, value)) = line.split_once(':') else { continue };
        match name.trim().to_ascii_lowercase().as_str() {
            "range" => range = Some(value.trim().to_owned()),
            "content-length" => content_length = value.trim().parse().unwrap_or(0),
            _ => {}
        }
    }
    Some(Head { method, target, range, content_length })
}
