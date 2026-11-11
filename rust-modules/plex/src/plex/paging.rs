//! The one place a Plex request spells its paging parameters.
//!
//! PMS answers a long listing in windows: `X-Plex-Container-Start` is the offset of the first row
//! wanted and `X-Plex-Container-Size` how many rows. The two travel together (see `params.rs`: a
//! lone size is ignored), so every paged request is built here, and a gate in `ci/check-deps.sh`
//! keeps the two names from being spelled anywhere else in non-test Rust.

/// A window of a server listing: `size` rows starting at row `start`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PageReq {
    pub start: usize,
    pub size: usize,
}

impl PageReq {
    /// The window for the `i64` counts the Client methods take. A negative offset or size has no
    /// meaning on the wire, so it is sent as zero rather than as a wrapped `usize`.
    pub(super) fn window(start: i64, size: i64) -> Self {
        Self { start: start.max(0) as usize, size: size.max(0) as usize }
    }
}

/// `path` with the window's two parameters appended. A path that already carries a query string
/// takes `&`, a bare path takes `?`; the parameters always go last, in `Start`, `Size` order.
pub fn paged_path(path: &str, req: PageReq) -> String {
    let sep = if path.contains('?') { '&' } else { '?' };
    format!("{path}{sep}X-Plex-Container-Start={}&X-Plex-Container-Size={}", req.start, req.size)
}

/// `key` with any window parameters it already carries removed, every other part kept in order.
/// A provider's listing key can arrive with its own window (a hub's `…&X-Plex-Container-Size=12`),
/// and re-windowing it must replace that window rather than stack a second one on it.
pub(super) fn without_window(key: &str) -> String {
    let (path, query) = key.split_once('?').unwrap_or((key, ""));
    let query = query.split('&').filter(|part| !part.is_empty())
        .filter(|part| !matches!(part.split('=').next(), Some("X-Plex-Container-Start" | "X-Plex-Container-Size")))
        .collect::<Vec<_>>().join("&");
    if query.is_empty() { path.to_owned() } else { format!("{path}?{query}") }
}

/// The window a request's query string asks for, and the query with the window's parameters
/// removed (every other part kept in order). The reading side of [`paged_path`], for code that
/// ANSWERS a paged request instead of sending one (the demo build's loopback server). `None` when
/// the query carries no start or no size: the two travel together, and a lone size is ignored, as
/// PMS ignores it. A value that is not a non-negative integer counts as absent.
pub fn split_window(query: &str) -> (Option<PageReq>, String) {
    let (mut start, mut size) = (None, None);
    let mut rest = Vec::new();
    for part in query.split('&').filter(|part| !part.is_empty()) {
        let (name, value) = part.split_once('=').unwrap_or((part, ""));
        match name {
            "X-Plex-Container-Start" => start = value.parse::<usize>().ok(),
            "X-Plex-Container-Size" => size = value.parse::<usize>().ok(),
            _ => rest.push(part),
        }
    }
    let window = match (start, size) {
        (Some(start), Some(size)) => Some(PageReq { start, size }),
        _ => None,
    };
    (window, rest.join("&"))
}

#[cfg(test)]
mod tests {
    use super::{paged_path, split_window, PageReq};

    #[test]
    fn split_window_reads_back_what_paged_path_wrote() {
        let built = paged_path("/library/sections/7/all?sort=titleSort", PageReq { start: 40, size: 20 });
        let query = built.split_once('?').unwrap().1;
        assert_eq!(split_window(query), (Some(PageReq { start: 40, size: 20 }), "sort=titleSort".to_owned()));
    }

    #[test]
    fn a_lone_size_is_no_window_and_is_still_removed() {
        assert_eq!(split_window("type=1&X-Plex-Container-Size=12"), (None, "type=1".to_owned()));
        assert_eq!(split_window(""), (None, String::new()));
    }

    #[test]
    fn a_bare_path_takes_the_window_after_a_question_mark() {
        assert_eq!(
            paged_path("/library/sections/7/collections", PageReq { start: 40, size: 20 }),
            "/library/sections/7/collections?X-Plex-Container-Start=40&X-Plex-Container-Size=20",
        );
    }

    #[test]
    fn a_path_with_a_query_takes_the_window_after_an_ampersand() {
        assert_eq!(
            paged_path("/library/sections/7/all?sort=addedAt%3Adesc", PageReq { start: 0, size: 60 }),
            "/library/sections/7/all?sort=addedAt%3Adesc&X-Plex-Container-Start=0&X-Plex-Container-Size=60",
        );
    }
}
