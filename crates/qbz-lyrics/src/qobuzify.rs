//! Qobuzify lyrics: the lyric server behind the Qobuzify apps
//! (`api.qobuzify.app`). One request per track returns pre-aligned lyrics:
//! word-synced ("Syllable") for most tracks, line-synced ("Line") or unsynced
//! ("Static") otherwise. ZBQ asks it first; the original chain (Qobuz, LRCLIB,
//! lyrics.ovh) is the fallback.
//!
//! The lyric object is kept exactly as the server sent it: Lyra, the renderer
//! the lyrics views use, reads it directly (syllable timing, duets, background
//! vocals). [`to_doc`] builds this crate's document from it for everything else
//! (the classic views, the miniplayer, copy-lyrics, the cache's LRC column).

use std::time::Duration;

use reqwest::Client;
use serde_json::Value;

use crate::model::{LyricsDoc, LyricsLine, LyricsProvider, Word};

const ENDPOINT: &str = "https://api.qobuzify.app/v1/lyrics";
const REFETCH_ENDPOINT: &str = "https://api.qobuzify.app/v1/refetch";
/// Where the server's codename for the source of a sheet is kept, inside the
/// stored lyric object (the object's own `source` is not that codename).
const SOURCE_KEY: &str = "zbqSource";

/// What a lyrics lookup asked the server.
#[derive(Debug, Clone, Default)]
pub struct QobuzifyQuery {
    pub title: String,
    pub artist: String,
    pub album: Option<String>,
    pub duration_secs: Option<u64>,
    pub isrc: Option<String>,
    /// Source codenames this user turned down for this track ("Wrong lyrics?").
    pub exclude: Vec<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum QobuzifyOutcome {
    /// The lyric object, JSON as the server sent it.
    Found(String),
    /// The server answered: no lyrics for this track (or an instrumental).
    Miss,
    /// No usable answer (offline, timeouts, server errors).
    Unreachable,
}

/// A "Wrong lyrics?" request's answer.
#[derive(Debug, Clone, PartialEq)]
pub enum RefetchOutcome {
    /// Another version: the lyric object, as in [`QobuzifyOutcome::Found`].
    Alternative(String),
    /// The server has no other version of these lyrics.
    OnlyVersion,
    /// Rate limited.
    TryLater,
    /// No usable answer.
    Failed,
}

fn client() -> Option<Client> {
    Client::builder()
        .user_agent("ZBQ")
        .build()
        .map_err(|e| log::warn!("[Lyrics] Qobuzify: no HTTP client: {e}"))
        .ok()
}

/// The request parameters both routes take (`qz=1` marks a real client
/// request; the server's WAF lets those through).
fn params(query: &QobuzifyQuery) -> Vec<(&'static str, String)> {
    let mut params: Vec<(&'static str, String)> = vec![
        ("qz", "1".to_string()),
        ("name", query.title.clone()),
        ("artist", query.artist.clone()),
    ];
    if let Some(album) = query.album.as_ref().filter(|a| !a.trim().is_empty()) {
        params.push(("album", album.clone()));
    }
    if let Some(secs) = query.duration_secs.filter(|s| *s > 0) {
        params.push(("durationMs", (secs * 1000).to_string()));
    }
    if let Some(isrc) = query.isrc.as_ref().filter(|i| !i.trim().is_empty()) {
        params.push(("isrc", isrc.clone()));
    }
    if !query.exclude.is_empty() {
        params.push(("exclude", query.exclude.join(",")));
    }
    params
}

/// "Wrong lyrics?": ask for a version other than `served` (the codename of the
/// sheet on screen) and everything in `query.exclude`.
pub async fn refetch_qobuzify(query: &QobuzifyQuery, served: Option<&str>) -> RefetchOutcome {
    if query.title.trim().is_empty() || query.artist.trim().is_empty() {
        return RefetchOutcome::Failed;
    }
    let Some(client) = client() else {
        return RefetchOutcome::Failed;
    };
    let mut params = params(query);
    if let Some(served) = served.filter(|s| !s.is_empty()) {
        params.push(("served", served.to_string()));
    }
    let response = client
        .get(REFETCH_ENDPOINT)
        .query(&params)
        .timeout(Duration::from_secs(15))
        .send()
        .await;
    match response {
        Ok(response) => {
            let status = response.status().as_u16();
            let body = response.json::<Value>().await.ok();
            classify_refetch(status, body.as_ref())
        }
        Err(e) => {
            log::debug!("[Lyrics] Qobuzify refetch failed: {e}");
            RefetchOutcome::Failed
        }
    }
}

/// One refetch answer: `{ok, alternative, source, lyrics}`, or HTTP 429.
pub fn classify_refetch(status: u16, body: Option<&Value>) -> RefetchOutcome {
    if status == 429 {
        return RefetchOutcome::TryLater;
    }
    let Some(body) = body.filter(|_| (200..300).contains(&status)) else {
        return RefetchOutcome::Failed;
    };
    if !flag(body, "ok") {
        return RefetchOutcome::Failed;
    }
    match body.get("alternative").and_then(Value::as_bool) {
        Some(true) => match body.get("lyrics") {
            Some(lyrics) if to_doc(&lyrics.to_string()).is_some() => {
                RefetchOutcome::Alternative(with_source(lyrics, body.get("source")))
            }
            _ => RefetchOutcome::Failed,
        },
        Some(false) => RefetchOutcome::OnlyVersion,
        None => RefetchOutcome::Failed,
    }
}

/// The lyric object with the answer's source codename kept inside it.
fn with_source(lyrics: &Value, source: Option<&Value>) -> String {
    let mut lyrics = lyrics.clone();
    if let (Some(object), Some(source)) = (
        lyrics.as_object_mut(),
        source.and_then(Value::as_str).filter(|s| !s.is_empty()),
    ) {
        object.insert(SOURCE_KEY.to_string(), Value::String(source.to_string()));
    }
    lyrics.to_string()
}

/// The source codename kept in a stored lyric object (see [`with_source`]).
pub fn served_source(raw: &str) -> Option<String> {
    serde_json::from_str::<Value>(raw)
        .ok()?
        .get(SOURCE_KEY)
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
}

/// Ask the server, with the Qobuzify desktop client's budget: a cold track fans
/// out to several upstreams server-side and can take 6-9 s, and an answer that
/// timed out is usually in the server's cache for the retry. A server that
/// answered "still working" (`recheck`/`errored`) gets two more tries; one that
/// could not be reached gets one.
pub async fn fetch_qobuzify(query: &QobuzifyQuery) -> QobuzifyOutcome {
    if query.title.trim().is_empty() || query.artist.trim().is_empty() {
        return QobuzifyOutcome::Miss;
    }
    let Some(client) = client() else {
        return QobuzifyOutcome::Unreachable;
    };
    let params = params(query);

    let mut unreachable = 0;
    for attempt in 0..3u32 {
        let timeout = Duration::from_secs(if attempt == 0 { 10 } else { 8 });
        let body: Option<Value> = match client.get(ENDPOINT).query(&params).timeout(timeout).send().await {
            Ok(response) if response.status().is_success() => response.json().await.ok(),
            Ok(response) => {
                log::debug!("[Lyrics] Qobuzify answered HTTP {}", response.status());
                None
            }
            Err(e) => {
                log::debug!("[Lyrics] Qobuzify request failed: {e}");
                None
            }
        };
        match body {
            None => {
                unreachable += 1;
                if unreachable >= 2 {
                    return QobuzifyOutcome::Unreachable;
                }
            }
            Some(body) => {
                let transient = flag(&body, "recheck") || flag(&body, "errored");
                if !transient || attempt == 2 {
                    return classify(&body);
                }
            }
        }
        if attempt < 2 {
            tokio::time::sleep(Duration::from_millis(if attempt == 0 { 1500 } else { 2500 })).await;
        }
    }
    QobuzifyOutcome::Unreachable
}

fn flag(body: &Value, key: &str) -> bool {
    body.get(key).and_then(Value::as_bool).unwrap_or(false)
}

/// One server answer: `{ok, hasLyrics, lyrics: {Type, Content | Lines}, ...}`.
pub fn classify(body: &Value) -> QobuzifyOutcome {
    if !flag(body, "ok") {
        return QobuzifyOutcome::Unreachable;
    }
    if !flag(body, "hasLyrics") {
        return QobuzifyOutcome::Miss;
    }
    match body.get("lyrics") {
        Some(lyrics) if to_doc(&lyrics.to_string()).is_some() => {
            QobuzifyOutcome::Found(with_source(lyrics, body.get("source")))
        }
        _ => QobuzifyOutcome::Miss,
    }
}

/// Zero-width characters some providers leave in lyric text: invisible, but
/// they split words and make blank-looking lines.
fn clean(text: &str) -> String {
    text.chars()
        .filter(|c| !matches!(c, '\u{200B}'..='\u{200D}' | '\u{2060}' | '\u{FEFF}'))
        .collect::<String>()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

fn ms(value: Option<&Value>) -> Option<i64> {
    value.and_then(Value::as_f64).filter(|s| s.is_finite() && *s >= 0.0).map(|s| (s * 1000.0).round() as i64)
}

/// Syllables to words: `IsPartOfWord` joins a syllable to the NEXT one.
fn words_of(syllables: &[Value]) -> Vec<Word> {
    let mut words: Vec<Word> = Vec::new();
    let mut joining = false;
    for (i, syllable) in syllables.iter().enumerate() {
        let text = clean(syllable.get("Text").and_then(Value::as_str).unwrap_or(""));
        let Some(start) = ms(syllable.get("StartTime")) else { continue };
        let end = ms(syllable.get("EndTime"))
            .or_else(|| syllables.get(i + 1).and_then(|n| ms(n.get("StartTime"))))
            .unwrap_or(start)
            .max(start);
        if text.is_empty() {
            continue;
        }
        match words.last_mut() {
            Some(word) if joining => {
                word.text.push_str(&text);
                word.end = word.end.max(end);
            }
            _ => words.push(Word { start, end, text }),
        }
        joining = syllable.get("IsPartOfWord").and_then(Value::as_bool).unwrap_or(false);
    }
    words
}

/// The crate's document for a Qobuzify lyric object (`None` when it holds no
/// lines). `native_json` keeps the object for Lyra.
pub fn to_doc(raw: &str) -> Option<LyricsDoc> {
    let lyrics: Value = serde_json::from_str(raw).ok()?;
    let kind = lyrics.get("Type").and_then(Value::as_str).unwrap_or("");
    let mut lines: Vec<LyricsLine> = Vec::new();
    let mut synced = true;
    if kind == "Static" || lyrics.get("Content").and_then(Value::as_array).map_or(true, |c| c.is_empty()) {
        synced = false;
        for row in lyrics.get("Lines").and_then(Value::as_array).into_iter().flatten() {
            let text = clean(row.get("Text").and_then(Value::as_str).unwrap_or(""));
            if !text.is_empty() {
                lines.push(LyricsLine { time_ms: None, end_ms: None, text, words: None });
            }
        }
    } else {
        for item in lyrics.get("Content").and_then(Value::as_array).into_iter().flatten() {
            let lead = item.get("Lead");
            let syllables = lead.and_then(|l| l.get("Syllables")).and_then(Value::as_array);
            if kind == "Syllable" && syllables.is_some() {
                let words = words_of(syllables.map(Vec::as_slice).unwrap_or(&[]));
                if words.is_empty() {
                    continue;
                }
                let text = words.iter().map(|w| w.text.as_str()).collect::<Vec<_>>().join(" ");
                let start = ms(lead.and_then(|l| l.get("StartTime"))).unwrap_or(words[0].start);
                let end = ms(lead.and_then(|l| l.get("EndTime")))
                    .unwrap_or(0)
                    .max(words.last().map_or(0, |w| w.end));
                lines.push(LyricsLine { time_ms: Some(start), end_ms: Some(end), text, words: Some(words) });
            } else {
                let text = clean(
                    item.get("Text")
                        .or_else(|| lead.and_then(|l| l.get("Text")))
                        .and_then(Value::as_str)
                        .unwrap_or(""),
                );
                let Some(start) = ms(item.get("StartTime")).or_else(|| ms(lead.and_then(|l| l.get("StartTime")))) else {
                    continue;
                };
                if text.is_empty() {
                    continue;
                }
                let end = ms(item.get("EndTime")).or_else(|| ms(lead.and_then(|l| l.get("EndTime"))));
                lines.push(LyricsLine { time_ms: Some(start), end_ms: end.filter(|e| *e > start), text, words: None });
            }
        }
        lines.sort_by_key(|l| l.time_ms.unwrap_or(0));
    }
    if lines.is_empty() {
        return None;
    }
    let mut doc = LyricsDoc::empty(LyricsProvider::Qobuzify);
    doc.lines = lines;
    doc.synced = synced;
    doc.native_json = Some(raw.to_string());
    Some(doc)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn syllable_doc() -> String {
        json!({
            "Type": "Syllable",
            "Content": [
                { "Type": "Vocal", "OppositeAligned": false, "Lead": {
                    "StartTime": 27.395, "EndTime": 30.1,
                    "Syllables": [
                        { "Text": "I", "StartTime": 27.395, "EndTime": 27.549, "IsPartOfWord": false },
                        { "Text": "been", "StartTime": 27.549, "EndTime": 27.74, "IsPartOfWord": false },
                        { "Text": "try", "StartTime": 27.74, "EndTime": 27.9, "IsPartOfWord": true },
                        { "Text": "na\u{200B}", "StartTime": 27.9, "EndTime": 28.077, "IsPartOfWord": false },
                        { "Text": "call", "StartTime": 28.077, "EndTime": 28.96, "IsPartOfWord": false }
                    ] },
                  "Background": [ { "StartTime": 29.0, "EndTime": 30.0, "Syllables": [
                        { "Text": "(ooh)", "StartTime": 29.0, "EndTime": 30.0, "IsPartOfWord": false } ] } ] },
                { "Type": "Vocal", "OppositeAligned": true, "Lead": {
                    "StartTime": 31.0, "EndTime": 33.0,
                    "Syllables": [ { "Text": "Maybe", "StartTime": 31.0, "IsPartOfWord": false },
                                   { "Text": "you", "StartTime": 31.6, "EndTime": 33.0, "IsPartOfWord": false } ] } }
            ]
        })
        .to_string()
    }

    #[test]
    fn syllables_become_words_and_lines() {
        let raw = syllable_doc();
        let doc = to_doc(&raw).expect("doc");
        assert_eq!(doc.provider, LyricsProvider::Qobuzify);
        assert!(doc.synced);
        assert_eq!(doc.native_json.as_deref(), Some(raw.as_str()), "kept verbatim for Lyra");
        assert_eq!(doc.lines.len(), 2);
        let first = &doc.lines[0];
        // IsPartOfWord joins "try" + "na"; the zero-width space is gone; the
        // background vocal is Lyra's business, not the classic line's.
        assert_eq!(first.text, "I been tryna call");
        assert_eq!((first.time_ms, first.end_ms), (Some(27_395), Some(30_100)));
        let words = first.words.as_ref().expect("words");
        assert_eq!(words.iter().map(|w| w.text.as_str()).collect::<Vec<_>>(), ["I", "been", "tryna", "call"]);
        assert_eq!((words[2].start, words[2].end), (27_740, 28_077));
        // A missing EndTime runs to the next syllable.
        let second = doc.lines[1].words.as_ref().expect("words");
        assert_eq!((second[0].start, second[0].end), (31_000, 31_600));
    }

    #[test]
    fn line_and_static_sheets() {
        let line = json!({ "Type": "Line", "Content": [
            { "Type": "Vocal", "Text": "second", "StartTime": 12.5, "EndTime": 14.0 },
            { "Type": "Vocal", "Text": "first", "StartTime": 10.0 },
            { "Type": "Vocal", "Text": "  ", "StartTime": 11.0 } ] })
        .to_string();
        let doc = to_doc(&line).expect("doc");
        assert!(doc.synced);
        assert_eq!(doc.lines.iter().map(|l| l.text.as_str()).collect::<Vec<_>>(), ["first", "second"]);
        assert_eq!(doc.lines[0].end_ms, None);
        assert_eq!(doc.lines[1].end_ms, Some(14_000));
        assert!(doc.lines.iter().all(|l| l.words.is_none()));

        let stat = json!({ "Type": "Static", "Lines": [ { "Text": "one" }, { "Text": "" }, { "Text": "two  words" } ] }).to_string();
        let doc = to_doc(&stat).expect("doc");
        assert!(!doc.synced);
        assert_eq!(doc.plain_text(), "one\ntwo words");

        assert!(to_doc(&json!({ "Type": "Syllable", "Content": [] }).to_string()).is_none());
        assert!(to_doc("not json").is_none());
    }

    /// The real server (`cargo test -p qbz-lyrics -- --ignored live_`): a
    /// well-known track comes back word-synced through the same client code.
    #[tokio::test]
    #[ignore = "network: asks api.qobuzify.app"]
    async fn live_qobuzify_lookup() {
        let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
        let query = QobuzifyQuery {
            title: "Blinding Lights".into(),
            artist: "The Weeknd".into(),
            album: Some("After Hours".into()),
            duration_secs: Some(200),
            isrc: None,
            exclude: Vec::new(),
        };
        let QobuzifyOutcome::Found(raw) = fetch_qobuzify(&query).await else {
            panic!("no lyrics from the live server");
        };
        let doc = to_doc(&raw).expect("parses");
        assert!(doc.synced && doc.lines.len() > 20, "{} lines", doc.lines.len());
        assert!(doc.lines.iter().all(|l| l.words.is_some()), "word-synced");
        let miss = QobuzifyQuery { title: "zbq no such song qx7".into(), artist: "nobody qx7".into(), ..Default::default() };
        assert_eq!(fetch_qobuzify(&miss).await, QobuzifyOutcome::Miss);
    }

    #[test]
    fn server_answers() {
        let found = json!({ "ok": true, "hasLyrics": true, "source": "Ne Plus Ultra", "lyrics": serde_json::from_str::<Value>(&syllable_doc()).unwrap() });
        let QobuzifyOutcome::Found(raw) = classify(&found) else { panic!("found") };
        assert_eq!(served_source(&raw).as_deref(), Some("Ne Plus Ultra"), "the codename rides along");
        assert!(to_doc(&raw).is_some(), "and the sheet still parses");
        assert_eq!(classify(&json!({ "ok": true, "hasLyrics": false })), QobuzifyOutcome::Miss);
        assert_eq!(classify(&json!({ "ok": true, "hasLyrics": true, "lyrics": { "Type": "Line", "Content": [] } })), QobuzifyOutcome::Miss);
        assert_eq!(classify(&json!({ "ok": false })), QobuzifyOutcome::Unreachable);
    }

    #[test]
    fn refetch_answers() {
        let lyrics = serde_json::from_str::<Value>(&syllable_doc()).unwrap();
        let alt = json!({ "ok": true, "alternative": true, "source": "Coloratura", "previousSource": "Ne Plus Ultra", "lyrics": lyrics });
        let RefetchOutcome::Alternative(raw) = classify_refetch(200, Some(&alt)) else { panic!("alternative") };
        assert_eq!(served_source(&raw).as_deref(), Some("Coloratura"));
        assert_eq!(classify_refetch(200, Some(&json!({ "ok": true, "alternative": false }))), RefetchOutcome::OnlyVersion);
        assert_eq!(classify_refetch(429, None), RefetchOutcome::TryLater);
        assert_eq!(classify_refetch(500, Some(&alt)), RefetchOutcome::Failed);
        assert_eq!(classify_refetch(200, Some(&json!({ "ok": false }))), RefetchOutcome::Failed);
        assert_eq!(classify_refetch(200, Some(&json!({ "ok": true, "alternative": true, "lyrics": { "Type": "Line", "Content": [] } }))), RefetchOutcome::Failed);
    }

    #[test]
    fn exclusions_ride_on_the_request() {
        let query = QobuzifyQuery { title: "t".into(), artist: "a".into(), exclude: vec!["Ne Plus Ultra".into(), "Coloratura".into()], ..Default::default() };
        let sent = params(&query);
        assert!(sent.contains(&("exclude", "Ne Plus Ultra,Coloratura".to_string())));
        let none = params(&QobuzifyQuery { title: "t".into(), artist: "a".into(), ..Default::default() });
        assert!(none.iter().all(|(k, _)| *k != "exclude"));
    }
}
