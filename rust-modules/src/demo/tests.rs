use super::bundle::{normalize, pick, range, window_slice, parse_head, Answer, Entry, Index, Keys, Variant, PHOTO};
use plx_plex::plex::{split_window, PageReq};

fn keys(full: &str, loose: &str, bare: &str) -> Keys {
    Keys { full: full.into(), loose: loose.into(), bare: bare.into() }
}

/// The vectors `tools/demo_bundle.py --selftest` pins (VECTORS there): the bundle is written with
/// that normaliser and read with this one, so they must agree on every one.
#[test]
fn the_normaliser_agrees_with_the_bundle_writer() {
    let cases: [(&str, &str, Keys); 6] = [
        ("GET", "/library/sections", keys("GET /library/sections", "GET /library/sections", "GET /library/sections")),
        ("HEAD", "/identity?X-Plex-Token=abc", keys("GET /identity", "GET /identity", "GET /identity")),
        (
            "GET",
            "/library/sections/1/all?type=1&sort=titleSort&X-Plex-Container-Start=0&X-Plex-Container-Size=50&X-Plex-Client-Identifier=c",
            keys("GET /library/sections/1/all?sort=titleSort&type=1", "GET /library/sections/1/all?type=1", "GET /library/sections/1/all"),
        ),
        (
            "POST",
            "/playQueues?type=video&uri=server%3A%2F%2Fm%2Fcom.plexapp.plugins.library%2Flibrary%2Fmetadata%2F120&continuous=1&shuffle=0&repeat=0&X-Plex-Session-Identifier=s",
            keys(
                "POST /playQueues?continuous=1&repeat=0&shuffle=0&type=video&uri=server%3A%2F%2Fm%2Fcom.plexapp.plugins.library%2Flibrary%2Fmetadata%2F120",
                "POST /playQueues?type=video&uri=server%3A%2F%2Fm%2Fcom.plexapp.plugins.library%2Flibrary%2Fmetadata%2F120",
                "POST /playQueues",
            ),
        ),
        (
            "GET",
            "/photo/:/transcode?width=250&height=375&minSize=1&url=%2Flibrary%2Fmetadata%2F42%2Fthumb%2F1",
            keys(
                "GET /photo/:/transcode?url=%2Flibrary%2Fmetadata%2F42%2Fthumb%2F1",
                "GET /photo/:/transcode?url=%2Flibrary%2Fmetadata%2F42%2Fthumb%2F1",
                "GET /photo/:/transcode",
            ),
        ),
        (
            "GET",
            "/hubs/search?query=big+buck&limit=3&includeCollections=1&accessToken=x",
            keys("GET /hubs/search?includeCollections=1&limit=3&query=big%20buck", "GET /hubs/search?query=big%20buck", "GET /hubs/search"),
        ),
    ];
    for (method, target, want) in cases {
        assert_eq!(normalize(method, target), want, "{method} {target}");
    }
}

fn entry(file: &str) -> Entry {
    Entry { status: 200, ctype: "application/json".into(), file: file.into() }
}

fn variant(w: u32, h: u32) -> Variant {
    Variant { w, h, ctype: "image/jpeg".into(), file: format!("{w}x{h}.jpg") }
}

#[test]
fn a_request_falls_back_from_the_full_key_to_the_loose_one_and_then_the_path() {
    let mut index = Index::default();
    index.entries.insert("GET /library/sections/1/all?sort=titleSort&type=1".into(), entry("full"));
    index.entries.insert("GET /library/sections/1/all?type=1".into(), entry("loose"));
    index.entries.insert("GET /library/sections/1/all".into(), entry("bare"));
    let file = |q: &str| match index.answer("GET", "/library/sections/1/all", q) {
        Answer::Saved { file, .. } => file.to_owned(),
        other => panic!("{other:?}"),
    };
    assert_eq!(file("type=1&sort=titleSort&X-Plex-Client-Identifier=c"), "full");
    assert_eq!(file("type=1&sort=addedAt%3Adesc"), "loose");
    assert_eq!(file("genre=7"), "bare");
}

#[test]
fn an_unsaved_read_is_a_miss_and_an_unsaved_write_is_acknowledged() {
    let index = Index::default();
    assert_eq!(index.answer("GET", "/library/metadata/9", "includeChildren=1"),
        Answer::Miss("GET /library/metadata/9?includeChildren=1".into()));
    assert_eq!(index.answer("HEAD", "/library/metadata/9", ""), Answer::Miss("GET /library/metadata/9".into()));
    assert_eq!(index.answer("POST", "/:/timeline", "state=playing"), Answer::Ack);
    assert_eq!(index.answer("PUT", "/library/parts/120", "audioStreamID=1"), Answer::Ack);
}

#[test]
fn an_image_is_the_smallest_stored_size_that_covers_the_box() {
    let v = [variant(240, 360), variant(480, 720), variant(960, 1440)];
    assert_eq!(pick(&v, 250, 375).unwrap().w, 480);
    assert_eq!(pick(&v, 200, 300).unwrap().w, 240);
    assert_eq!(pick(&v, 0, 0).unwrap().w, 240, "no size asked: the smallest");
    assert_eq!(pick(&v, 1920, 1080).unwrap().w, 960, "nothing covers it: the largest");
    assert!(pick(&[], 1, 1).is_none());
}

#[test]
fn a_photo_transcode_is_answered_from_the_image_variants_by_its_url() {
    let mut index = Index::default();
    index.images.insert("/library/metadata/42/thumb/1".into(), vec![variant(240, 360), variant(480, 720)]);
    let got = index.answer("GET", PHOTO, "width=300&height=450&minSize=1&url=%2Flibrary%2Fmetadata%2F42%2Fthumb%2F1");
    assert_eq!(got, Answer::Saved { status: 200, ctype: "image/jpeg", file: "480x720.jpg" });
    assert!(matches!(index.answer("GET", PHOTO, "width=1&height=1&url=%2Fnone"), Answer::Miss(_)));
}

#[test]
fn a_window_is_cut_out_of_a_whole_listing() {
    let body = br#"{"MediaContainer":{"size":5,"Metadata":[{"r":0},{"r":1},{"r":2},{"r":3},{"r":4}]}}"#;
    let cut: serde_json::Value = serde_json::from_slice(&window_slice(body, PageReq { start: 1, size: 2 }).unwrap()).unwrap();
    let mc = &cut["MediaContainer"];
    assert_eq!(mc["Metadata"], serde_json::json!([{"r":1},{"r":2}]));
    assert_eq!((mc["offset"].as_u64(), mc["size"].as_u64(), mc["totalSize"].as_u64()), (Some(1), Some(2), Some(5)));
    let past: serde_json::Value = serde_json::from_slice(&window_slice(body, PageReq { start: 9, size: 2 }).unwrap()).unwrap();
    assert_eq!(past["MediaContainer"]["size"].as_u64(), Some(0));
    assert!(window_slice(b"not json", PageReq { start: 0, size: 1 }).is_none());
    assert!(window_slice(br#"{"MediaContainer":{"size":0}}"#, PageReq { start: 0, size: 1 }).is_none());
}

#[test]
fn the_window_comes_from_the_one_paging_owner() {
    let (window, rest) = split_window("type=1&X-Plex-Container-Start=10&X-Plex-Container-Size=5");
    assert_eq!(window, Some(PageReq { start: 10, size: 5 }));
    assert_eq!(rest, "type=1");
}

#[test]
fn ranges_are_read_as_a_media_source_sends_them() {
    assert_eq!(range(None, 100), Ok(None));
    assert_eq!(range(Some("bytes=0-9"), 100), Ok(Some((0, 9))));
    assert_eq!(range(Some("bytes=90-"), 100), Ok(Some((90, 99))));
    assert_eq!(range(Some("bytes=-10"), 100), Ok(Some((90, 99))));
    assert_eq!(range(Some("bytes=50-500"), 100), Ok(Some((50, 99))));
    assert_eq!(range(Some("bytes=100-"), 100), Err(()));
    assert_eq!(range(Some("bytes=0-1,5-6"), 100), Ok(None));
    assert_eq!(range(Some("items=0-1"), 100), Ok(None));
}

#[test]
fn a_request_head_yields_its_line_range_and_length() {
    let h = parse_head("GET /library/parts/1/file.mp4?X-Plex-Token=t HTTP/1.1\r\nHost: 127.0.0.1\r\nrange: bytes=0-\r\nContent-Length: 0").unwrap();
    assert_eq!((h.method.as_str(), h.target.as_str(), h.range.as_deref(), h.content_length),
        ("GET", "/library/parts/1/file.mp4?X-Plex-Token=t", Some("bytes=0-"), 0));
    assert!(parse_head("garbage").is_none());
    assert!(parse_head("GET http://elsewhere/ HTTP/1.1").is_none());
}
