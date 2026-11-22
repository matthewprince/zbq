//! Lyrics orchestrator — the Qobuz-first resolution chain (spec §1.1).
//!
//! ```text
//! get(LyricsRequest)
//! ├─ 0. OFFLINE? -> CACHE-ONLY: serve any cached entry (synced or plain);
//! │       miss -> NotAvailableOffline (deviation D3, mission-blessed).
//! ├─ 1. CACHE probe: by track_id, then by cache_key.
//! ├─ 1b. ZBQ: QOBUZIFY (api.qobuzify.app) runs alongside 2-3 and wins with
//! │       synced lyrics: served when it answers, or up to QOBUZIFY_GRACE after
//! │       the rest of the chain; a later answer replaces the served entry and
//! │       reaches the frontend through the upgrade hook. A synced cache hit
//! │       from another provider asks it once per run, the same way.
//! │       synced hit -> serve. plain-only hit -> SOFT MISS, continue
//! │       (uniform for ALL providers including qobuz — Q4).
//! ├─ 2. QOBUZ (PRIMARY) — only for Qobuz-source tracks with a real track_id:
//! │       wsync -> SYNCED, provider=qobuz -> upsert (+native wsync json,
//! │       amended Q5), serve. DONE.
//! │       plain -> HELD as candidate while LRCLIB is probed for synced
//! │       (the no-sync-regression rule, §1.5).
//! │       miss / any error -> silent degradation, continue.
//! ├─ 3. LRCLIB: search-first + scorer, exactly 1 retry on transport error.
//! │       synced -> serve (provider=lrclib).
//! │       plain-only with a held qobuz-plain -> prefer the QOBUZ plain.
//! ├─ 4. lyrics.ovh (plain-only): only if nothing held so far.
//! └─ 5. UPSERT whatever was served; nothing -> NotFound.
//! ```
//!
//! Fix-forwards baked in: in-flight dedupe keyed by request (F6), request-key
//! echo on every response for the caller's stale guard (F2), typed offline
//! status instead of a hardcoded string (F3), explicit raw-title-to-providers
//! contract (F4 — the `version` field never existed here to be dropped).

use std::collections::{HashMap, HashSet};
use std::future::Future;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::{Arc, Mutex as StdMutex};
use std::time::Duration;

use async_trait::async_trait;
use futures_util::future::{FutureExt, Shared};
use tokio::sync::Mutex;

use qbz_qobuz::{QobuzClient, QobuzLyricsContent, QobuzLyricsDocument};

use crate::cache::{CachedLyrics, LyricsCacheDb, LyricsCacheStats};
use crate::model::{
    build_cache_key, derive_has_translation, LyricsDoc, LyricsPayload, LyricsProvider,
};
use crate::lrc::{emit_lrc, LrcSpan};
use crate::providers::{fetch_lrclib, fetch_lyrics_ovh, LyricsData};
use crate::qobuzify::{fetch_qobuzify, refetch_qobuzify, served_source, QobuzifyOutcome, QobuzifyQuery, RefetchOutcome};
use crate::wsync::{translated_from_content, QobuzWsync};

/// What kind of source the playing track comes from. The Qobuz primary step
/// only applies to Qobuz catalog tracks with a real track_id; local-library /
/// Plex / offline-local tracks go straight to the external fallback chain.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LyricsSourceKind {
    Qobuz,
    NonQobuz,
}

/// One lyrics lookup request. `offline` is data, not a lookup — the crate
/// never reaches into a frontend's offline store (spec §2.2.4); the glue
/// passes the engine verdict.
#[derive(Debug, Clone)]
pub struct LyricsRequest {
    pub track_id: Option<u64>,
    pub source: LyricsSourceKind,
    pub title: String,
    pub artist: String,
    pub album: Option<String>,
    pub duration_secs: Option<u64>,
    pub offline: bool,
    /// Active translation target (ISO 639-1, Qobuz API v10). `None` = the
    /// default original-only fetch; `Some(lang)` asks the Qobuz primary for
    /// a translation and drives the client-derived `has_translation` on the
    /// served document. Only the Qobuz provider is affected — the external
    /// fallback chain never sees it.
    pub language: Option<String>,
    /// The recording's ISRC when known: lets Qobuzify tell same-title
    /// recordings apart. Only Qobuzify uses it.
    pub isrc: Option<String>,
}

/// Served lyrics: the wire-compatible payload plus the parsed document
/// (structured lines; native words when the source was Qobuz wsync).
#[derive(Debug, Clone, PartialEq)]
pub struct LyricsResult {
    pub payload: LyricsPayload,
    pub doc: LyricsDoc,
}

#[derive(Debug, Clone, PartialEq)]
pub enum LyricsOutcome {
    Found(LyricsResult),
    NotFound,
    /// Offline and nothing cached — the UI maps this to a translated string
    /// (fix F3; never a hardcoded message).
    NotAvailableOffline,
}

/// Response envelope. `request_track_id`/`request_key` echo the request so
/// callers can discard responses for tracks no longer current (fixes the
/// Tauri stale-response race F2 by construction).
#[derive(Debug, Clone, PartialEq)]
pub struct LyricsResponse {
    pub request_track_id: Option<u64>,
    pub request_key: String,
    pub outcome: LyricsOutcome,
}

/// Provider boundary: the real HTTP lives behind this trait so tests inject
/// fakes and the chain logic stays headless-testable.
#[async_trait]
pub trait LyricsProviders: Send + Sync {
    /// Qobuz primary (two-step: signed lyricsUrl + CDN doc). `language` is the
    /// optional translation target (ISO 639-1, v10); `None` fetches
    /// original-only. `Ok(None)` = typed miss; `Err` = transport/auth/offline
    /// failure — both degrade silently to the fallback chain.
    async fn qobuz(
        &self,
        track_id: u64,
        language: Option<&str>,
    ) -> Result<Option<QobuzLyricsDocument>, String>;

    /// LRCLIB. `Ok(None)` = no match; `Err` = transport error (the chain
    /// retries exactly once).
    async fn lrclib(
        &self,
        title: &str,
        artist: &str,
        duration_secs: Option<u64>,
    ) -> Result<Option<LyricsData>, String>;

    /// lyrics.ovh, plain-only.
    async fn ovh(&self, title: &str, artist: &str) -> Option<LyricsData>;

    /// Qobuzify (`api.qobuzify.app`), ZBQ's first source. The default asks
    /// nothing, so test fakes and other frontends keep the original chain.
    async fn qobuzify(&self, _query: QobuzifyQuery) -> QobuzifyOutcome {
        QobuzifyOutcome::Miss
    }

    /// Qobuzify's "Wrong lyrics?" route: another version than `served`. The
    /// default answers nothing.
    async fn qobuzify_refetch(&self, _query: QobuzifyQuery, _served: Option<String>) -> RefetchOutcome {
        RefetchOutcome::Failed
    }
}

/// Production providers: Qobuz via the shared client, externals via the
/// verbatim-ported fetchers.
pub struct HttpLyricsProviders {
    client: Arc<QobuzClient>,
}

impl HttpLyricsProviders {
    pub fn new(client: Arc<QobuzClient>) -> Self {
        Self { client }
    }
}

#[async_trait]
impl LyricsProviders for HttpLyricsProviders {
    async fn qobuz(
        &self,
        track_id: u64,
        language: Option<&str>,
    ) -> Result<Option<QobuzLyricsDocument>, String> {
        self.client
            .get_lyrics(track_id, language)
            .await
            .map_err(|e| e.to_string())
    }

    async fn lrclib(
        &self,
        title: &str,
        artist: &str,
        duration_secs: Option<u64>,
    ) -> Result<Option<LyricsData>, String> {
        fetch_lrclib(title, artist, duration_secs).await
    }

    async fn ovh(&self, title: &str, artist: &str) -> Option<LyricsData> {
        fetch_lyrics_ovh(title, artist).await
    }

    async fn qobuzify(&self, query: QobuzifyQuery) -> QobuzifyOutcome {
        fetch_qobuzify(&query).await
    }

    async fn qobuzify_refetch(&self, query: QobuzifyQuery, served: Option<String>) -> RefetchOutcome {
        refetch_qobuzify(&query, served.as_deref()).await
    }
}

struct ServiceInner {
    providers: Arc<dyn LyricsProviders>,
    db: Mutex<Option<LyricsCacheDb>>,
    /// See [`LyricsService::on_upgrade`].
    upgraded: StdMutex<Option<UpgradeHook>>,
    /// Cache keys Qobuzify was already asked about in this run.
    qobuzify_asked: StdMutex<HashSet<String>>,
    /// See [`Rejections`].
    rejected: StdMutex<Rejections>,
}

/// Qobuzify source codenames this user turned down per track ("Wrong
/// lyrics?"), kept in `<cache dir>/lyrics/qobuzify_rejected.json` and sent as
/// `exclude` on later Qobuzify requests for that track, so a cache miss cannot
/// bring a rejected sheet back. Bounded like the Qobuzify desktop client's
/// list: 200 tracks (least recently touched dropped), 8 sources each.
#[derive(Default)]
struct Rejections {
    path: Option<PathBuf>,
    tracks: HashMap<String, RejectedTrack>,
}

#[derive(Clone, serde::Serialize, serde::Deserialize)]
struct RejectedTrack {
    /// Source codenames, oldest first.
    s: Vec<String>,
    /// Last touched, ms since the epoch.
    t: u64,
}

#[derive(serde::Serialize, serde::Deserialize)]
struct RejectionFile {
    v: u32,
    t: HashMap<String, RejectedTrack>,
}

const REJECTED_TRACKS_MAX: usize = 200;
const REJECTED_PER_TRACK_MAX: usize = 8;

impl Rejections {
    fn load(path: PathBuf) -> Self {
        let tracks = std::fs::read_to_string(&path)
            .ok()
            .and_then(|text| serde_json::from_str::<RejectionFile>(&text).ok())
            .filter(|file| file.v == 1)
            .map(|file| file.t)
            .unwrap_or_default();
        Self { path: Some(path), tracks }
    }

    fn of(&self, key: &str) -> Vec<String> {
        self.tracks.get(key).map(|track| track.s.clone()).unwrap_or_default()
    }

    /// Remember `source` as rejected for `key`. "Qobuzify" is the server's
    /// unmapped fallback and names nothing, so it is never stored.
    fn add(&mut self, key: &str, source: &str) {
        if source.is_empty() || source.eq_ignore_ascii_case("qobuzify") {
            return;
        }
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0);
        let track = self.tracks.entry(key.to_string()).or_insert(RejectedTrack { s: Vec::new(), t: now });
        track.s.retain(|s| s != source);
        track.s.push(source.to_string());
        if track.s.len() > REJECTED_PER_TRACK_MAX {
            let drop = track.s.len() - REJECTED_PER_TRACK_MAX;
            track.s.drain(..drop);
        }
        track.t = now;
        if self.tracks.len() > REJECTED_TRACKS_MAX {
            let mut keys: Vec<(String, u64)> = self.tracks.iter().map(|(k, v)| (k.clone(), v.t)).collect();
            keys.sort_by_key(|(_, t)| *t);
            for (k, _) in keys.into_iter().take(self.tracks.len() - REJECTED_TRACKS_MAX) {
                self.tracks.remove(&k);
            }
        }
        self.save();
    }

    fn save(&self) {
        let Some(path) = self.path.as_ref() else { return };
        let file = RejectionFile { v: 1, t: self.tracks.clone() };
        match serde_json::to_string(&file) {
            Ok(text) => {
                if let Err(e) = std::fs::write(path, text) {
                    log::warn!("[Lyrics] could not save the rejected lyric sources: {e}");
                }
            }
            Err(e) => log::warn!("[Lyrics] could not encode the rejected lyric sources: {e}"),
        }
    }
}

/// What a "Wrong lyrics?" request did.
#[derive(Debug, Clone)]
pub enum RefetchResult {
    /// Another version, cached in place of the old one; the response is for
    /// the frontend to show.
    Switched(LyricsResponse),
    /// The server has no other version of these lyrics.
    OnlyVersion,
    /// Rate limited: try again in a moment.
    TryLater,
    /// No usable answer.
    Failed,
}

type SharedLyricsFuture =
    Shared<Pin<Box<dyn Future<Output = Result<LyricsResponse, String>> + Send>>>;

/// The lyrics service. Owns the per-user cache handle
/// (`init_at`/`teardown`, mirroring Tauri's `LyricsState`) and the in-flight
/// dedupe map; safe to share across tasks/frontends via `Arc`.
pub struct LyricsService {
    inner: Arc<ServiceInner>,
    inflight: StdMutex<HashMap<String, SharedLyricsFuture>>,
}

impl LyricsService {
    pub fn new(providers: Arc<dyn LyricsProviders>) -> Self {
        Self {
            inner: Arc::new(ServiceInner {
                providers,
                db: Mutex::new(None),
                upgraded: StdMutex::new(None),
                qobuzify_asked: StdMutex::new(HashSet::new()),
                rejected: StdMutex::new(Rejections::default()),
            }),
            inflight: StdMutex::new(HashMap::new()),
        }
    }

    /// The frontend's hook for Qobuzify answers that arrive after their
    /// request was served: the entry is already replaced in the cache; the
    /// hook decides whether it is still for the track on screen.
    pub fn on_upgrade(&self, hook: impl Fn(LyricsResponse) + Send + Sync + 'static) {
        *self.inner.upgraded.lock().expect("upgrade hook poisoned") = Some(Arc::new(hook));
    }

    /// Convenience constructor over the production HTTP providers.
    pub fn with_qobuz_client(client: Arc<QobuzClient>) -> Self {
        Self::new(Arc::new(HttpLyricsProviders::new(client)))
    }

    /// Open the per-user cache at `<base_dir>/lyrics/lyrics.db` — the SAME
    /// file Tauri uses (call on session activation with the user cache dir).
    pub async fn init_at(&self, base_dir: &Path) -> Result<(), String> {
        let cache_dir = base_dir.join("lyrics");
        std::fs::create_dir_all(&cache_dir)
            .map_err(|e| format!("Failed to create lyrics cache directory: {}", e))?;
        let db_path = cache_dir.join("lyrics.db");
        let new_db = LyricsCacheDb::new(&db_path)?;
        let mut guard = self.inner.db.lock().await;
        *guard = Some(new_db);
        *self.inner.rejected.lock().expect("rejections poisoned") =
            Rejections::load(cache_dir.join("qobuzify_rejected.json"));
        Ok(())
    }

    /// Drop the cache handle (call on logout).
    pub async fn teardown(&self) {
        let mut guard = self.inner.db.lock().await;
        *guard = None;
        *self.inner.rejected.lock().expect("rejections poisoned") = Rejections::default();
    }

    /// "Wrong lyrics?": ask Qobuzify for another version of `request`'s lyrics
    /// than the one cached (its codename travels as `served`, with everything
    /// this user rejected before as `exclude`). Another version replaces the
    /// cached entry whatever its sync type (the user's pick sticks), and the
    /// rejected source is remembered for later requests.
    pub async fn refetch_qobuzify(&self, request: LyricsRequest) -> Result<RefetchResult, String> {
        if request.offline || request.title.trim().is_empty() || request.artist.trim().is_empty() {
            return Ok(RefetchResult::Failed);
        }
        let cache_key = build_cache_key(request.title.trim(), request.artist.trim(), request.duration_secs);
        let served = {
            let guard = self.inner.db.lock().await;
            let db = guard.as_ref().ok_or(NO_SESSION)?;
            db.get_by_cache_key(&cache_key)?
                .and_then(|cached| cached.qobuz_wsync_json)
                .and_then(|raw| served_source(&raw))
        };
        let mut query = qobuzify_query(&request);
        query.exclude = self.inner.rejected.lock().expect("rejections poisoned").of(&cache_key);
        if let Some(served) = served.as_ref().filter(|s| !query.exclude.contains(s)) {
            query.exclude.push(served.clone());
        }
        match self.inner.providers.qobuzify_refetch(query, served.clone()).await {
            RefetchOutcome::Alternative(raw) => {
                let base = LyricsPayload {
                    track_id: request.track_id,
                    title: request.title.trim().to_string(),
                    artist: request.artist.trim().to_string(),
                    album: request.album.clone(),
                    duration_secs: request.duration_secs,
                    plain: None,
                    synced_lrc: None,
                    provider: LyricsProvider::Qobuzify,
                    cached: false,
                };
                let Some((result, raw)) = qobuzify_result(&raw, base) else {
                    return Ok(RefetchResult::Failed);
                };
                {
                    let guard = self.inner.db.lock().await;
                    let db = guard.as_ref().ok_or(NO_SESSION)?;
                    db.upsert(&cache_key, &result.payload, Some(&raw))?;
                }
                if let Some(served) = served.as_deref() {
                    self.inner.rejected.lock().expect("rejections poisoned").add(&cache_key, served);
                }
                // Nothing this run may ask Qobuzify over the user's pick.
                self.inner
                    .qobuzify_asked
                    .lock()
                    .expect("qobuzify set poisoned")
                    .insert(cache_key.clone());
                log::info!("[Lyrics] Wrong lyrics?: switched to another version");
                Ok(RefetchResult::Switched(LyricsResponse {
                    request_track_id: request.track_id,
                    request_key: cache_key,
                    outcome: LyricsOutcome::Found(result),
                }))
            }
            RefetchOutcome::OnlyVersion => Ok(RefetchResult::OnlyVersion),
            RefetchOutcome::TryLater => Ok(RefetchResult::TryLater),
            RefetchOutcome::Failed => Ok(RefetchResult::Failed),
        }
    }

    /// Clear the per-user lyrics cache.
    pub async fn clear_cache(&self) -> Result<(), String> {
        let guard = self.inner.db.lock().await;
        let db = guard.as_ref().ok_or(NO_SESSION)?;
        db.clear()
    }

    /// Entries + size, BOTH from the per-user DB (fix F1).
    pub async fn cache_stats(&self) -> Result<LyricsCacheStats, String> {
        let guard = self.inner.db.lock().await;
        let db = guard.as_ref().ok_or(NO_SESSION)?;
        db.stats()
    }

    /// Resolve lyrics for a request through the §1.1 chain. Concurrent calls
    /// for the same request join one in-flight resolution (F6).
    pub async fn get(&self, request: LyricsRequest) -> Result<LyricsResponse, String> {
        let dedupe_key = dedupe_key(&request);

        let (future, is_owner) = {
            let mut inflight = self.inflight.lock().expect("inflight mutex poisoned");
            if let Some(existing) = inflight.get(&dedupe_key) {
                (existing.clone(), false)
            } else {
                let inner = Arc::clone(&self.inner);
                let boxed: Pin<
                    Box<dyn Future<Output = Result<LyricsResponse, String>> + Send>,
                > = Box::pin(run_chain(inner, request));
                let future = boxed.shared();
                inflight.insert(dedupe_key.clone(), future.clone());
                (future, true)
            }
        };

        let result = future.clone().await;

        if is_owner {
            let mut inflight = self.inflight.lock().expect("inflight mutex poisoned");
            if let Some(existing) = inflight.get(&dedupe_key) {
                if existing.ptr_eq(&future) {
                    inflight.remove(&dedupe_key);
                }
            }
        }

        result
    }
}

const NO_SESSION: &str = "No active session - please log in";

fn dedupe_key(request: &LyricsRequest) -> String {
    let identity = match request.track_id {
        Some(id) => format!("id:{}", id),
        None => format!(
            "key:{}",
            build_cache_key(
                request.title.trim(),
                request.artist.trim(),
                request.duration_secs
            )
        ),
    };
    // The translation target is part of the identity: a refetch WITH a
    // language must never join an in-flight original-only resolution.
    format!(
        "{}|offline:{}|lang:{}",
        identity,
        request.offline,
        request.language.as_deref().unwrap_or("")
    )
}

/// A cached entry satisfies a language request only when its stored native
/// wsync document carries a translation FOR THAT language (the content's own
/// `lang` records it, spec §A.5); anything else falls through to a refetch.
/// Original-only requests (`None`) are served by any synced hit, as before.
fn cached_serves_language(cached: &CachedLyrics, language: Option<&str>) -> bool {
    match language {
        None => true,
        Some(lang) => cached
            .qobuz_wsync_json
            .as_deref()
            .and_then(|json| serde_json::from_str::<QobuzWsync>(json).ok())
            .and_then(|wsync| wsync.translation)
            .and_then(|content| content.lang().map(str::to_owned))
            .map(|cached_lang| cached_lang == lang)
            .unwrap_or(false),
    }
}

fn cached_result(cached: CachedLyrics, requested_lang: Option<&str>) -> LyricsResult {
    let doc = LyricsDoc::from_cached(
        &cached.payload,
        cached.qobuz_wsync_json.as_deref(),
        requested_lang,
    );
    LyricsResult {
        payload: cached.payload,
        doc,
    }
}

// ---------------------------------------------------------------------------
// Qobuzify (ZBQ): asked first, alongside the chain
// ---------------------------------------------------------------------------

/// Called with a Qobuzify answer that arrived after its request was already
/// served from the fallback chain (or from a cached fallback entry).
pub type UpgradeHook = Arc<dyn Fn(LyricsResponse) + Send + Sync>;

/// How much longer than the rest of the chain a Qobuzify answer is waited for
/// before the chain's own result is served (a later answer still lands).
const QOBUZIFY_GRACE: Duration = Duration::from_millis(2500);

fn qobuzify_query(request: &LyricsRequest) -> QobuzifyQuery {
    QobuzifyQuery {
        title: request.title.trim().to_string(),
        artist: request.artist.trim().to_string(),
        album: request.album.clone(),
        duration_secs: request.duration_secs,
        isrc: request.isrc.clone(),
        exclude: Vec::new(),
    }
}

/// Whether to ask Qobuzify: not offline (cache only), not for a translation
/// (only the Qobuz primary has those), and once per track per run.
fn should_ask_qobuzify(inner: &ServiceInner, request: &LyricsRequest, cache_key: &str) -> bool {
    if request.offline || request.language.is_some() {
        return false;
    }
    inner
        .qobuzify_asked
        .lock()
        .expect("qobuzify set poisoned")
        .insert(cache_key.to_string())
}

fn spawn_qobuzify(
    inner: &ServiceInner,
    request: &LyricsRequest,
    cache_key: &str,
) -> tokio::task::JoinHandle<QobuzifyOutcome> {
    let providers = Arc::clone(&inner.providers);
    let mut query = qobuzify_query(request);
    query.exclude = inner.rejected.lock().expect("rejections poisoned").of(cache_key);
    tokio::spawn(async move { providers.qobuzify(query).await })
}

/// The served result for a Qobuzify lyric object, and the JSON to cache with it
/// (in the native-document column the Qobuz wsync rows use).
fn qobuzify_result(raw: &str, base: LyricsPayload) -> Option<(LyricsResult, String)> {
    let doc = crate::qobuzify::to_doc(raw)?;
    let mut payload = base;
    payload.provider = LyricsProvider::Qobuzify;
    payload.plain = Some(doc.plain_text());
    payload.synced_lrc = doc.synced.then(|| {
        let spans: Vec<LrcSpan> = doc
            .lines
            .iter()
            .filter_map(|line| {
                Some(LrcSpan {
                    start_ms: line.time_ms?,
                    end_ms: line.end_ms,
                    text: line.text.clone(),
                })
            })
            .collect();
        emit_lrc(&spans)
    });
    Some((LyricsResult { payload, doc }, raw.to_string()))
}

/// Cache and return a Qobuzify answer with synced lyrics (`None` when it has
/// no sync: the chain goes on, and the sheet is the last resort).
async fn serve_qobuzify(
    inner: &ServiceInner,
    cache_key: &str,
    raw: &str,
    base: LyricsPayload,
) -> Result<Option<LyricsResult>, String> {
    let Some((result, raw)) = qobuzify_result(raw, base) else {
        return Ok(None);
    };
    if !result.doc.synced {
        return Ok(None);
    }
    let guard = inner.db.lock().await;
    let db = guard.as_ref().ok_or(NO_SESSION)?;
    db.upsert(cache_key, &result.payload, Some(&raw))?;
    Ok(Some(result))
}

/// A Qobuzify answer that arrives after its request was served: it replaces the
/// cached entry and goes to the upgrade hook (the frontend swaps it in).
fn finish_qobuzify_later(
    inner: Arc<ServiceInner>,
    request: LyricsRequest,
    cache_key: String,
    task: tokio::task::JoinHandle<QobuzifyOutcome>,
) {
    tokio::spawn(async move {
        let Ok(QobuzifyOutcome::Found(raw)) = task.await else {
            return;
        };
        let base = LyricsPayload {
            track_id: request.track_id,
            title: request.title.trim().to_string(),
            artist: request.artist.trim().to_string(),
            album: request.album.clone(),
            duration_secs: request.duration_secs,
            plain: None,
            synced_lrc: None,
            provider: LyricsProvider::Qobuzify,
            cached: false,
        };
        let result = match serve_qobuzify(&inner, &cache_key, &raw, base).await {
            Ok(Some(result)) => result,
            Ok(None) => return,
            Err(e) => {
                log::warn!("[Lyrics] caching the late Qobuzify answer failed: {e}");
                return;
            }
        };
        log::info!("[Lyrics] Qobuzify answered after the request was served; swapping it in");
        let hook = inner.upgraded.lock().expect("upgrade hook poisoned").clone();
        if let Some(hook) = hook {
            hook(LyricsResponse {
                request_track_id: request.track_id,
                request_key: cache_key,
                outcome: LyricsOutcome::Found(result),
            });
        }
    });
}

async fn run_chain(
    inner: Arc<ServiceInner>,
    request: LyricsRequest,
) -> Result<LyricsResponse, String> {
    let title = request.title.trim().to_string();
    let artist = request.artist.trim().to_string();

    if title.is_empty() || artist.is_empty() {
        // Parity with v2_lyrics_get (`legacy_compat.rs:479-486`).
        return Err("Lyrics lookup requires title and artist".to_string());
    }

    let cache_key = build_cache_key(&title, &artist, request.duration_secs);
    let respond = |outcome: LyricsOutcome| LyricsResponse {
        request_track_id: request.track_id,
        request_key: cache_key.clone(),
        outcome,
    };

    // 0. OFFLINE -> cache-only: serve ANY cached entry (synced or plain — no
    //    soft-miss offline, there is nothing to upgrade from); miss -> typed
    //    NotAvailableOffline. External providers are SKIPPED entirely.
    if request.offline {
        let guard = inner.db.lock().await;
        let db = guard.as_ref().ok_or(NO_SESSION)?;
        let cached = match request.track_id {
            Some(id) => db.get_by_track_id(id).ok().flatten(),
            None => None,
        }
        .or_else(|| db.get_by_cache_key(&cache_key).ok().flatten());

        return Ok(match cached {
            Some(cached) => respond(LyricsOutcome::Found(cached_result(
                cached,
                request.language.as_deref(),
            ))),
            None => respond(LyricsOutcome::NotAvailableOffline),
        });
    }

    // 1. Cache probe: by track_id first, then by key. A plain-only entry is a
    //    SOFT MISS and falls through the chain — uniform for every provider
    //    including 'qobuz' (Q4: preserves the synced-upgrade path).
    {
        let guard = inner.db.lock().await;
        let db = guard.as_ref().ok_or(NO_SESSION)?;
        let cached = match request.track_id {
            Some(id) => db.get_by_track_id(id).ok().flatten(),
            None => None,
        }
        .or_else(|| db.get_by_cache_key(&cache_key).ok().flatten());

        if let Some(cached) = cached {
            let has_synced = cached
                .payload
                .synced_lrc
                .as_ref()
                .map(|s| !s.trim().is_empty())
                .unwrap_or(false);
            if has_synced && cached_serves_language(&cached, request.language.as_deref()) {
                if cached.payload.provider != LyricsProvider::Qobuzify
                    && should_ask_qobuzify(&inner, &request, &cache_key)
                {
                    let task = spawn_qobuzify(&inner, &request, &cache_key);
                    finish_qobuzify_later(Arc::clone(&inner), request.clone(), cache_key.clone(), task);
                }
                return Ok(respond(LyricsOutcome::Found(cached_result(
                    cached,
                    request.language.as_deref(),
                ))));
            }
            // plain-only cache — or a synced entry without a translation for
            // the requested language: fall through to re-fetch (spec §A.5).
        }
    } // lock released before any network round-trip

    let base_payload = |provider: LyricsProvider| LyricsPayload {
        track_id: request.track_id,
        title: title.clone(),
        artist: artist.clone(),
        album: request.album.clone(),
        duration_secs: request.duration_secs,
        plain: None,
        synced_lrc: None,
        provider,
        cached: false,
    };

    // 1b. Qobuzify, alongside 2+3 (see the module docs).
    let mut qobuzify_task = should_ask_qobuzify(&inner, &request, &cache_key)
        .then(|| spawn_qobuzify(&inner, &request, &cache_key));
    let mut qobuzify_sheet: Option<String> = None;

    // 2+3. QOBUZ primary + LRCLIB run CONCURRENTLY. Running them in parallel
    //       means a Qobuz MISS adds NO latency before LRCLIB (matching Tauri's
    //       speed, which has no Qobuz step) while still preferring Qobuz wsync
    //       when present. The SELECTION below is unchanged from the sequential
    //       version: Qobuz wsync wins outright; otherwise an LRCLIB *synced*
    //       result beats a held Qobuz-plain (no-sync-regression), which beats
    //       an LRCLIB plain-only / miss; lyrics.ovh is the last resort. Qobuz
    //       only runs for Qobuz-source tracks with a real track_id.
    let qobuz_fut = async {
        if request.source == LyricsSourceKind::Qobuz {
            if let Some(track_id) = request.track_id {
                match inner
                    .providers
                    .qobuz(track_id, request.language.as_deref())
                    .await
                {
                    Ok(Some(document)) => return Some(document),
                    Ok(None) => log::debug!("[Lyrics] Qobuz miss for track {}", track_id),
                    Err(e) => {
                        // Offline gate / transport / auth — degrade like a
                        // miss; never a user-facing error (spec §1.2).
                        log::debug!("[Lyrics] Qobuz degraded for track {}: {}", track_id, e)
                    }
                }
            }
        }
        None
    };
    // LRCLIB with exactly 1 retry on transport error (parity:
    // legacy_compat.rs:519-536 — Ok(None) is a miss, NOT a retry).
    let lrclib_fut = async {
        match inner.providers.lrclib(&title, &artist, request.duration_secs).await {
            Ok(data) => data,
            Err(e) => {
                log::warn!("[Lyrics] LRCLIB attempt 1 failed: {}, retrying…", e);
                match inner.providers.lrclib(&title, &artist, request.duration_secs).await {
                    Ok(data) => data,
                    Err(e2) => {
                        log::warn!("[Lyrics] LRCLIB attempt 2 failed: {}, falling back", e2);
                        None
                    }
                }
            }
        }
    };
    let chain = async { tokio::join!(qobuz_fut, lrclib_fut) };
    tokio::pin!(chain);
    let (qobuz_doc, lrclib_data) = loop {
        let Some(task) = qobuzify_task.as_mut() else {
            break (&mut chain).await;
        };
        let answer = tokio::select! {
            done = &mut chain => break done,
            answer = task => answer,
        };
        qobuzify_task = None;
        if let Ok(QobuzifyOutcome::Found(raw)) = answer {
            if let Some(found) = serve_qobuzify(&inner, &cache_key, &raw, base_payload(LyricsProvider::Qobuzify)).await? {
                return Ok(respond(LyricsOutcome::Found(found)));
            }
            qobuzify_sheet = Some(raw);
        }
    };
    if let Some(mut task) = qobuzify_task.take() {
        match tokio::time::timeout(QOBUZIFY_GRACE, &mut task).await {
            Ok(Ok(QobuzifyOutcome::Found(raw))) => {
                if let Some(found) = serve_qobuzify(&inner, &cache_key, &raw, base_payload(LyricsProvider::Qobuzify)).await? {
                    return Ok(respond(LyricsOutcome::Found(found)));
                }
                qobuzify_sheet = Some(raw);
            }
            Ok(_) => {}
            Err(_) => finish_qobuzify_later(Arc::clone(&inner), request.clone(), cache_key.clone(), task),
        }
    }

    // Apply the Qobuz result: wsync ends the chain; plain is HELD as a
    // candidate while we consider an LRCLIB synced result below.
    let mut qobuz_plain_candidate: Option<LyricsResult> = None;
    if let Some(document) = qobuz_doc {
        if let Some(wsync) = QobuzWsync::from_document(&document) {
            let doc = wsync.to_doc(request.language.as_deref());
            if !doc.lines.is_empty() {
                let mut payload = base_payload(LyricsProvider::Qobuz);
                payload.plain = Some(doc.plain_text());
                payload.synced_lrc = Some(wsync.to_lrc());
                let wsync_json = serde_json::to_string(&wsync)
                    .map_err(|e| format!("Failed to serialize wsync: {}", e))?;

                let guard = inner.db.lock().await;
                let db = guard.as_ref().ok_or(NO_SESSION)?;
                db.upsert(&cache_key, &payload, Some(&wsync_json))?;

                return Ok(respond(LyricsOutcome::Found(LyricsResult { payload, doc })));
            }
        } else if let Some(QobuzLyricsContent::Plain { lines, .. }) = document.original.as_ref() {
            let blob = lines
                .iter()
                .map(|line| line.line.as_str())
                .collect::<Vec<_>>()
                .join("\n");
            if !blob.trim().is_empty() {
                let mut doc = LyricsDoc::from_plain_text(&blob, LyricsProvider::Qobuz);
                doc.translation_langs = document.translation_langs.clone();
                doc.writers = document.writers.clone();
                doc.translation = document
                    .translation
                    .as_ref()
                    .map(|content| Box::new(translated_from_content(content)));
                doc.has_translation = derive_has_translation(
                    &document.translation_langs,
                    request.language.as_deref(),
                );
                let mut payload = base_payload(LyricsProvider::Qobuz);
                payload.plain = Some(blob.trim().to_string());
                qobuz_plain_candidate = Some(LyricsResult { payload, doc });
            }
        }
    }

    if let Some(data) = lrclib_data {
        let has_synced = data
            .synced_lrc
            .as_ref()
            .map(|s| !s.trim().is_empty())
            .unwrap_or(false);
        // Plain-only LRCLIB while a qobuz-plain candidate is held -> prefer
        // the QOBUZ plain (first-party, line-split natively — §1.1 step 3).
        if has_synced || qobuz_plain_candidate.is_none() {
            let mut payload = base_payload(data.provider);
            payload.plain = data.plain;
            payload.synced_lrc = data.synced_lrc;
            let doc = LyricsDoc::from_payload(&payload);

            let guard = inner.db.lock().await;
            let db = guard.as_ref().ok_or(NO_SESSION)?;
            db.upsert(&cache_key, &payload, None)?;

            return Ok(respond(LyricsOutcome::Found(LyricsResult { payload, doc })));
        }
    }

    // Held qobuz-plain candidate wins over an LRCLIB plain-only result and
    // over an LRCLIB miss; lyrics.ovh runs only when nothing is held (§1.1).
    if let Some(result) = qobuz_plain_candidate {
        let guard = inner.db.lock().await;
        let db = guard.as_ref().ok_or(NO_SESSION)?;
        db.upsert(&cache_key, &result.payload, None)?;
        return Ok(respond(LyricsOutcome::Found(result)));
    }

    // 4. lyrics.ovh (plain-only fallback #2).
    if let Some(data) = inner.providers.ovh(&title, &artist).await {
        let mut payload = base_payload(data.provider);
        payload.plain = data.plain;
        payload.synced_lrc = data.synced_lrc;
        let doc = LyricsDoc::from_payload(&payload);

        let guard = inner.db.lock().await;
        let db = guard.as_ref().ok_or(NO_SESSION)?;
        db.upsert(&cache_key, &payload, None)?;

        return Ok(respond(LyricsOutcome::Found(LyricsResult { payload, doc })));
    }

    // 5. Qobuzify's unsynced sheet, when it was all there was.
    if let Some((result, raw)) = qobuzify_sheet
        .as_deref()
        .and_then(|raw| qobuzify_result(raw, base_payload(LyricsProvider::Qobuzify)))
    {
        let guard = inner.db.lock().await;
        let db = guard.as_ref().ok_or(NO_SESSION)?;
        db.upsert(&cache_key, &result.payload, Some(&raw))?;
        return Ok(respond(LyricsOutcome::Found(result)));
    }

    Ok(respond(LyricsOutcome::NotFound))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::VecDeque;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Duration;

    // ---------- fakes ----------

    #[derive(Default)]
    struct FakeProviders {
        qobuz_queue: StdMutex<VecDeque<Result<Option<QobuzLyricsDocument>, String>>>,
        lrclib_queue: StdMutex<VecDeque<Result<Option<LyricsData>, String>>>,
        ovh_queue: StdMutex<VecDeque<Option<LyricsData>>>,
        qobuz_calls: AtomicUsize,
        lrclib_calls: AtomicUsize,
        ovh_calls: AtomicUsize,
        /// Qobuzify answers, each after its delay.
        qobuzify_queue: StdMutex<VecDeque<(u64, QobuzifyOutcome)>>,
        qobuzify_calls: AtomicUsize,
        /// The `exclude` list each Qobuzify lookup carried.
        qobuzify_excludes: StdMutex<Vec<Vec<String>>>,
        /// Refetch answers, and the (served, exclude) each refetch carried.
        refetch_queue: StdMutex<VecDeque<RefetchOutcome>>,
        refetch_calls: StdMutex<Vec<(Option<String>, Vec<String>)>>,
        /// Language argument observed on each qobuz call (threading proof).
        qobuz_languages: StdMutex<Vec<Option<String>>>,
        delay_ms: Option<u64>,
    }

    #[async_trait]
    impl LyricsProviders for FakeProviders {
        async fn qobuz(
            &self,
            _track_id: u64,
            language: Option<&str>,
        ) -> Result<Option<QobuzLyricsDocument>, String> {
            self.qobuz_calls.fetch_add(1, Ordering::SeqCst);
            self.qobuz_languages
                .lock()
                .unwrap()
                .push(language.map(str::to_owned));
            if let Some(ms) = self.delay_ms {
                tokio::time::sleep(Duration::from_millis(ms)).await;
            }
            self.qobuz_queue
                .lock()
                .unwrap()
                .pop_front()
                .unwrap_or(Ok(None))
        }

        async fn lrclib(
            &self,
            _title: &str,
            _artist: &str,
            _duration_secs: Option<u64>,
        ) -> Result<Option<LyricsData>, String> {
            self.lrclib_calls.fetch_add(1, Ordering::SeqCst);
            self.lrclib_queue
                .lock()
                .unwrap()
                .pop_front()
                .unwrap_or(Ok(None))
        }

        async fn ovh(&self, _title: &str, _artist: &str) -> Option<LyricsData> {
            self.ovh_calls.fetch_add(1, Ordering::SeqCst);
            self.ovh_queue.lock().unwrap().pop_front().unwrap_or(None)
        }

        async fn qobuzify(&self, query: QobuzifyQuery) -> QobuzifyOutcome {
            self.qobuzify_calls.fetch_add(1, Ordering::SeqCst);
            self.qobuzify_excludes.lock().unwrap().push(query.exclude.clone());
            let next = self.qobuzify_queue.lock().unwrap().pop_front();
            match next {
                Some((delay, outcome)) => {
                    tokio::time::sleep(Duration::from_millis(delay)).await;
                    outcome
                }
                None => QobuzifyOutcome::Miss,
            }
        }

        async fn qobuzify_refetch(&self, query: QobuzifyQuery, served: Option<String>) -> RefetchOutcome {
            self.refetch_calls.lock().unwrap().push((served, query.exclude.clone()));
            self.refetch_queue.lock().unwrap().pop_front().unwrap_or(RefetchOutcome::Failed)
        }
    }

    fn wsync_document() -> QobuzLyricsDocument {
        serde_json::from_str(
            r#"{
            "track_id": 100, "album_id": "alb",
            "translation_langs": ["es"], "writers": "W. Riter",
            "original": {"type": "wsync", "lang": "en", "lines": [
                {"line": "first line", "start": 1000, "end": 3000,
                 "words": [{"word": "first", "start": 1000, "end": 1800},
                           {"word": "line", "start": 1800, "end": 3000}]},
                {"line": "", "words": []},
                {"line": "second line", "start": 9000, "end": 11000,
                 "words": [{"word": "second", "start": 9000, "end": 9900},
                           {"word": "line", "start": 9900, "end": 11000}]}
            ]}
        }"#,
        )
        .unwrap()
    }

    fn wsync_document_with_translation() -> QobuzLyricsDocument {
        serde_json::from_str(
            r#"{
            "track_id": 100, "album_id": "alb",
            "translation_langs": ["es", "fr"], "writers": "W. Riter",
            "original": {"type": "wsync", "lang": "en", "lines": [
                {"line": "first line", "start": 1000, "end": 3000,
                 "words": [{"word": "first", "start": 1000, "end": 1800},
                           {"word": "line", "start": 1800, "end": 3000}]},
                {"line": "second line", "start": 9000, "end": 11000,
                 "words": [{"word": "second", "start": 9000, "end": 9900},
                           {"word": "line", "start": 9900, "end": 11000}]}
            ]},
            "translation": {"type": "wsync", "lang": "es", "lines": [
                {"line": "primera linea", "start": 1000, "end": 3000,
                 "words": [{"word": "primera", "start": 1000, "end": 1800},
                           {"word": "linea", "start": 1800, "end": 3000}]},
                {"line": "segunda linea", "start": 9000, "end": 11000,
                 "words": [{"word": "segunda", "start": 9000, "end": 9900},
                           {"word": "linea", "start": 9900, "end": 11000}]}
            ]}
        }"#,
        )
        .unwrap()
    }

    fn plain_document() -> QobuzLyricsDocument {
        serde_json::from_str(
            r#"{
            "track_id": 100, "album_id": "alb",
            "original": {"type": "plain", "lang": "en",
                "lines": [{"line": "plain one"}, {"line": "plain two"}]}
        }"#,
        )
        .unwrap()
    }

    fn lrclib_synced() -> LyricsData {
        LyricsData {
            plain: Some("ext one\next two".into()),
            synced_lrc: Some("[00:01.00] ext one\n[00:05.00] ext two".into()),
            provider: LyricsProvider::Lrclib,
        }
    }

    fn lrclib_plain() -> LyricsData {
        LyricsData {
            plain: Some("ext plain".into()),
            synced_lrc: None,
            provider: LyricsProvider::Lrclib,
        }
    }

    fn ovh_plain() -> LyricsData {
        LyricsData {
            plain: Some("ovh plain".into()),
            synced_lrc: None,
            provider: LyricsProvider::Ovh,
        }
    }

    fn request() -> LyricsRequest {
        LyricsRequest {
            track_id: Some(100),
            source: LyricsSourceKind::Qobuz,
            title: "Title".into(),
            artist: "Artist".into(),
            album: Some("Album".into()),
            duration_secs: Some(200),
            offline: false,
            language: None,
            isrc: None,
        }
    }

    async fn service_with(providers: Arc<FakeProviders>) -> (LyricsService, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let service = LyricsService::new(providers);
        service.init_at(dir.path()).await.unwrap();
        (service, dir)
    }

    fn found(response: &LyricsResponse) -> &LyricsResult {
        match &response.outcome {
            LyricsOutcome::Found(result) => result,
            other => panic!("expected Found, got {:?}", other),
        }
    }

    // ---------- chain priority ----------

    #[tokio::test]
    async fn qobuz_wsync_ends_the_chain() {
        let providers = Arc::new(FakeProviders::default());
        providers
            .qobuz_queue
            .lock()
            .unwrap()
            .push_back(Ok(Some(wsync_document())));
        let (service, _dir) = service_with(providers.clone()).await;

        let response = service.get(request()).await.unwrap();
        let result = found(&response);
        assert_eq!(result.payload.provider, LyricsProvider::Qobuz);
        assert!(result.doc.synced);
        assert_eq!(result.doc.lines.len(), 2);
        assert!(result.doc.lines[0].words.is_some(), "native words preserved");
        assert_eq!(result.doc.writers.as_deref(), Some("W. Riter"));
        // LRC emitted for cross-frontend compat; plain joined.
        assert!(result.payload.synced_lrc.as_deref().unwrap().contains("[00:01.000] first line"));
        assert_eq!(result.payload.plain.as_deref(), Some("first line\nsecond line"));
        // LRCLIB now runs CONCURRENTLY with Qobuz (parallel resolution), so it
        // is called once — but Qobuz wsync still wins the selection and its
        // result is discarded; ovh never runs once something is found.
        assert_eq!(providers.lrclib_calls.load(Ordering::SeqCst), 1);
        assert_eq!(providers.ovh_calls.load(Ordering::SeqCst), 0);
        // Stale-guard echo.
        assert_eq!(response.request_track_id, Some(100));
        assert_eq!(response.request_key, "artist::title::200");
    }

    #[tokio::test]
    async fn qobuz_wsync_is_cached_with_native_column_and_served_from_cache() {
        let providers = Arc::new(FakeProviders::default());
        providers
            .qobuz_queue
            .lock()
            .unwrap()
            .push_back(Ok(Some(wsync_document())));
        let (service, _dir) = service_with(providers.clone()).await;

        service.get(request()).await.unwrap();

        // Second call: synced cache hit — no provider calls at all.
        let response = service.get(request()).await.unwrap();
        let result = found(&response);
        assert!(result.payload.cached);
        assert_eq!(result.payload.provider, LyricsProvider::Qobuz);
        // Reader preferred the native wsync column: words survive the cache.
        assert!(result.doc.lines[0].words.is_some());
        assert_eq!(providers.qobuz_calls.load(Ordering::SeqCst), 1);
        // LRCLIB ran once on the FIRST call (parallel with Qobuz); the second
        // call was a synced cache hit, so it added no provider traffic.
        assert_eq!(providers.lrclib_calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn qobuz_plain_held_lrclib_synced_wins() {
        let providers = Arc::new(FakeProviders::default());
        providers
            .qobuz_queue
            .lock()
            .unwrap()
            .push_back(Ok(Some(plain_document())));
        providers
            .lrclib_queue
            .lock()
            .unwrap()
            .push_back(Ok(Some(lrclib_synced())));
        let (service, _dir) = service_with(providers.clone()).await;

        let response = service.get(request()).await.unwrap();
        let result = found(&response);
        // No-sync-regression rule: synced LRCLIB beats the held qobuz plain.
        assert_eq!(result.payload.provider, LyricsProvider::Lrclib);
        assert!(result.doc.synced);
        assert_eq!(providers.ovh_calls.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn qobuz_plain_preferred_over_lrclib_plain() {
        let providers = Arc::new(FakeProviders::default());
        providers
            .qobuz_queue
            .lock()
            .unwrap()
            .push_back(Ok(Some(plain_document())));
        providers
            .lrclib_queue
            .lock()
            .unwrap()
            .push_back(Ok(Some(lrclib_plain())));
        let (service, _dir) = service_with(providers.clone()).await;

        let response = service.get(request()).await.unwrap();
        let result = found(&response);
        assert_eq!(result.payload.provider, LyricsProvider::Qobuz);
        assert!(!result.doc.synced);
        assert_eq!(result.payload.plain.as_deref(), Some("plain one\nplain two"));
        // Something was held -> ovh never runs.
        assert_eq!(providers.ovh_calls.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn qobuz_plain_served_when_lrclib_misses_without_touching_ovh() {
        let providers = Arc::new(FakeProviders::default());
        providers
            .qobuz_queue
            .lock()
            .unwrap()
            .push_back(Ok(Some(plain_document())));
        // lrclib default = Ok(None) miss
        let (service, _dir) = service_with(providers.clone()).await;

        let response = service.get(request()).await.unwrap();
        let result = found(&response);
        assert_eq!(result.payload.provider, LyricsProvider::Qobuz);
        assert_eq!(providers.lrclib_calls.load(Ordering::SeqCst), 1);
        assert_eq!(providers.ovh_calls.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn full_fallback_to_ovh_then_not_found() {
        let providers = Arc::new(FakeProviders::default());
        providers.ovh_queue.lock().unwrap().push_back(Some(ovh_plain()));
        let (service, _dir) = service_with(providers.clone()).await;

        let response = service.get(request()).await.unwrap();
        let result = found(&response);
        assert_eq!(result.payload.provider, LyricsProvider::Ovh);
        assert!(!result.doc.synced);

        // ovh result is plain-only -> cached as plain -> soft miss -> chain
        // re-runs on the next call; with every provider missing now, NotFound.
        let response = service.get(request()).await.unwrap();
        assert_eq!(response.outcome, LyricsOutcome::NotFound);
        assert_eq!(response.request_key, "artist::title::200");
    }

    #[tokio::test]
    async fn qobuz_transport_error_degrades_silently() {
        let providers = Arc::new(FakeProviders::default());
        providers
            .qobuz_queue
            .lock()
            .unwrap()
            .push_back(Err("Offline mode is active".into()));
        providers
            .lrclib_queue
            .lock()
            .unwrap()
            .push_back(Ok(Some(lrclib_synced())));
        let (service, _dir) = service_with(providers.clone()).await;

        let response = service.get(request()).await.unwrap();
        assert_eq!(found(&response).payload.provider, LyricsProvider::Lrclib);
    }

    #[tokio::test]
    async fn lrclib_retries_exactly_once_on_transport_error() {
        // Err then Ok(Some) -> retry succeeds.
        let providers = Arc::new(FakeProviders::default());
        {
            let mut q = providers.lrclib_queue.lock().unwrap();
            q.push_back(Err("timeout".into()));
            q.push_back(Ok(Some(lrclib_synced())));
        }
        let (service, _dir) = service_with(providers.clone()).await;
        let response = service.get(request()).await.unwrap();
        assert_eq!(found(&response).payload.provider, LyricsProvider::Lrclib);
        assert_eq!(providers.lrclib_calls.load(Ordering::SeqCst), 2);

        // Err, Err -> falls to ovh (exactly 2 attempts, never 3).
        let providers = Arc::new(FakeProviders::default());
        {
            let mut q = providers.lrclib_queue.lock().unwrap();
            q.push_back(Err("timeout".into()));
            q.push_back(Err("timeout".into()));
        }
        providers.ovh_queue.lock().unwrap().push_back(Some(ovh_plain()));
        let (service, _dir) = service_with(providers.clone()).await;
        let response = service.get(request()).await.unwrap();
        assert_eq!(found(&response).payload.provider, LyricsProvider::Ovh);
        assert_eq!(providers.lrclib_calls.load(Ordering::SeqCst), 2);
    }

    // ---------- translation (v10) ----------

    #[tokio::test]
    async fn language_threads_to_qobuz_and_translation_is_mapped() {
        let providers = Arc::new(FakeProviders::default());
        providers
            .qobuz_queue
            .lock()
            .unwrap()
            .push_back(Ok(Some(wsync_document_with_translation())));
        let (service, _dir) = service_with(providers.clone()).await;

        let mut translated = request();
        translated.language = Some("es".into());
        let response = service.get(translated).await.unwrap();
        let result = found(&response);

        // The language reached the provider; the doc carries the mapped
        // translation (wsync -> WordSynced, native word stamps, same timings).
        assert_eq!(
            providers.qobuz_languages.lock().unwrap().as_slice(),
            &[Some("es".to_string())]
        );
        assert!(result.doc.has_translation);
        let translation = result.doc.translation.as_ref().expect("translation mapped");
        assert_eq!(translation.kind, crate::LyricsKind::WordSynced);
        assert_eq!(translation.lang.as_deref(), Some("es"));
        assert_eq!(translation.lines.len(), 2);
        assert_eq!(translation.lines[0].text, "primera linea");
        assert_eq!(translation.lines[0].time_ms, Some(1000));
        let words = translation.lines[0].words.as_ref().expect("words survive");
        assert_eq!(words[0].text, "primera");
        // Sync stays keyed on the ORIGINAL lines/timings.
        assert_eq!(result.doc.lines[0].text, "first line");
    }

    #[tokio::test]
    async fn translation_survives_the_cache_for_the_same_language() {
        let providers = Arc::new(FakeProviders::default());
        providers
            .qobuz_queue
            .lock()
            .unwrap()
            .push_back(Ok(Some(wsync_document_with_translation())));
        let (service, _dir) = service_with(providers.clone()).await;

        let mut translated = request();
        translated.language = Some("es".into());
        service.get(translated.clone()).await.unwrap();

        // Same language again -> synced cache hit WITH the stored
        // translation; no second provider call.
        let response = service.get(translated).await.unwrap();
        let result = found(&response);
        assert!(result.payload.cached);
        assert!(result.doc.has_translation);
        let translation = result.doc.translation.as_ref().expect("cached translation");
        assert_eq!(translation.lang.as_deref(), Some("es"));
        assert!(translation.lines[0].words.is_some());
        assert_eq!(providers.qobuz_calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn different_language_refetches_original_only_uses_the_cache() {
        let providers = Arc::new(FakeProviders::default());
        {
            let mut q = providers.qobuz_queue.lock().unwrap();
            q.push_back(Ok(Some(wsync_document_with_translation())));
            q.push_back(Ok(Some(wsync_document_with_translation())));
        }
        let (service, _dir) = service_with(providers.clone()).await;

        // Seed the cache with the es translation.
        let mut es = request();
        es.language = Some("es".into());
        service.get(es).await.unwrap();

        // A request for a DIFFERENT language misses the stored translation
        // and refetches (spec §A.5: the stored translation records its lang).
        let mut fr = request();
        fr.language = Some("fr".into());
        let response = service.get(fr).await.unwrap();
        assert!(!found(&response).payload.cached);
        assert_eq!(providers.qobuz_calls.load(Ordering::SeqCst), 2);

        // Original-only requests are still served by the synced cache hit.
        let response = service.get(request()).await.unwrap();
        let result = found(&response);
        assert!(result.payload.cached);
        assert!(!result.doc.has_translation);
        assert_eq!(providers.qobuz_calls.load(Ordering::SeqCst), 2);
        // The default fetch sent NO language.
        assert_eq!(
            providers.qobuz_languages.lock().unwrap().as_slice(),
            &[Some("es".to_string()), Some("fr".to_string())]
        );
    }

    #[tokio::test]
    async fn default_fetch_maps_original_only_even_when_langs_are_listed() {
        let providers = Arc::new(FakeProviders::default());
        providers
            .qobuz_queue
            .lock()
            .unwrap()
            .push_back(Ok(Some(wsync_document())));
        let (service, _dir) = service_with(providers.clone()).await;

        let response = service.get(request()).await.unwrap();
        let result = found(&response);
        // translation_langs passthrough present, but nothing was requested:
        // no translation, has_translation false.
        assert_eq!(result.doc.translation_langs, vec!["es"]);
        assert!(result.doc.translation.is_none());
        assert!(!result.doc.has_translation);
        assert_eq!(
            providers.qobuz_languages.lock().unwrap().as_slice(),
            &[None]
        );
    }

    #[tokio::test]
    async fn plain_original_with_plain_translation_is_mapped() {
        let providers = Arc::new(FakeProviders::default());
        providers
            .qobuz_queue
            .lock()
            .unwrap()
            .push_back(Ok(Some(serde_json::from_str(
                r#"{
                "track_id": 100, "album_id": "alb",
                "translation_langs": ["de"],
                "original": {"type": "plain", "lang": "en",
                    "lines": [{"line": "plain one"}, {"line": "plain two"}]},
                "translation": {"type": "plain", "lang": "de",
                    "lines": [{"line": "einfach eins"}, {"line": "einfach zwei"}]}
            }"#,
            )
            .unwrap())));
        let (service, _dir) = service_with(providers.clone()).await;

        let mut translated = request();
        translated.language = Some("de".into());
        let response = service.get(translated).await.unwrap();
        let result = found(&response);
        assert_eq!(result.payload.provider, LyricsProvider::Qobuz);
        assert!(!result.doc.synced);
        assert!(result.doc.has_translation);
        let translation = result.doc.translation.as_ref().expect("translation mapped");
        assert_eq!(translation.kind, crate::LyricsKind::Plain);
        assert_eq!(translation.lines.len(), 2);
        assert_eq!(translation.lines[0].text, "einfach eins");
    }

    // ---------- cache policy ----------

    #[tokio::test]
    async fn plain_only_cache_is_a_soft_miss_and_upgrades_to_synced() {
        let providers = Arc::new(FakeProviders::default());
        // 1st call: qobuz plain (held + served after lrclib miss).
        providers
            .qobuz_queue
            .lock()
            .unwrap()
            .push_back(Ok(Some(plain_document())));
        let (service, _dir) = service_with(providers.clone()).await;
        let response = service.get(request()).await.unwrap();
        assert_eq!(found(&response).payload.provider, LyricsProvider::Qobuz);
        assert!(!found(&response).doc.synced);

        // 2nd call: the qobuz-plain cache entry soft-misses (uniform rule,
        // Q4) — qobuz misses now, but LRCLIB has synced: upgrade happens.
        providers
            .lrclib_queue
            .lock()
            .unwrap()
            .push_back(Ok(Some(lrclib_synced())));
        let response = service.get(request()).await.unwrap();
        let result = found(&response);
        assert_eq!(result.payload.provider, LyricsProvider::Lrclib);
        assert!(result.doc.synced);
        assert!(!result.payload.cached);

        // 3rd call: synced cache hit, no more provider traffic.
        let lrclib_before = providers.lrclib_calls.load(Ordering::SeqCst);
        let response = service.get(request()).await.unwrap();
        assert!(found(&response).payload.cached);
        assert_eq!(providers.lrclib_calls.load(Ordering::SeqCst), lrclib_before);
    }

    #[tokio::test]
    async fn cache_probe_falls_back_from_track_id_to_key() {
        let providers = Arc::new(FakeProviders::default());
        providers
            .lrclib_queue
            .lock()
            .unwrap()
            .push_back(Ok(Some(lrclib_synced())));
        let (service, _dir) = service_with(providers.clone()).await;

        // Seed via a request WITHOUT track_id (keyed by metadata only).
        let mut seed = request();
        seed.track_id = None;
        seed.source = LyricsSourceKind::NonQobuz;
        service.get(seed).await.unwrap();

        // Same metadata, now WITH a track_id unknown to the cache: the id
        // probe misses, the key probe hits.
        let response = service.get(request()).await.unwrap();
        assert!(found(&response).payload.cached);
        assert_eq!(providers.lrclib_calls.load(Ordering::SeqCst), 1);
    }

    // ---------- offline ----------

    #[tokio::test]
    async fn offline_serves_cached_plain_and_skips_all_providers() {
        let providers = Arc::new(FakeProviders::default());
        providers.ovh_queue.lock().unwrap().push_back(Some(ovh_plain()));
        let (service, _dir) = service_with(providers.clone()).await;

        // Seed a plain-only entry online.
        service.get(request()).await.unwrap();
        let calls_before = (
            providers.qobuz_calls.load(Ordering::SeqCst),
            providers.lrclib_calls.load(Ordering::SeqCst),
            providers.ovh_calls.load(Ordering::SeqCst),
        );

        // Offline: cached PLAIN is served (no soft-miss offline), zero calls.
        let mut offline_request = request();
        offline_request.offline = true;
        let response = service.get(offline_request).await.unwrap();
        let result = found(&response);
        assert!(result.payload.cached);
        assert_eq!(result.payload.provider, LyricsProvider::Ovh);
        assert_eq!(
            (
                providers.qobuz_calls.load(Ordering::SeqCst),
                providers.lrclib_calls.load(Ordering::SeqCst),
                providers.ovh_calls.load(Ordering::SeqCst),
            ),
            calls_before
        );
    }

    #[tokio::test]
    async fn offline_miss_is_typed_not_available_offline() {
        let providers = Arc::new(FakeProviders::default());
        let (service, _dir) = service_with(providers.clone()).await;

        let mut offline_request = request();
        offline_request.offline = true;
        let response = service.get(offline_request).await.unwrap();
        assert_eq!(response.outcome, LyricsOutcome::NotAvailableOffline);
        assert_eq!(providers.qobuz_calls.load(Ordering::SeqCst), 0);
        assert_eq!(providers.lrclib_calls.load(Ordering::SeqCst), 0);
        assert_eq!(providers.ovh_calls.load(Ordering::SeqCst), 0);
    }

    // ---------- gating & validation ----------

    #[tokio::test]
    async fn non_qobuz_source_skips_the_qobuz_step() {
        let providers = Arc::new(FakeProviders::default());
        providers
            .lrclib_queue
            .lock()
            .unwrap()
            .push_back(Ok(Some(lrclib_synced())));
        let (service, _dir) = service_with(providers.clone()).await;

        let mut local_request = request();
        local_request.source = LyricsSourceKind::NonQobuz;
        let response = service.get(local_request).await.unwrap();
        assert_eq!(found(&response).payload.provider, LyricsProvider::Lrclib);
        assert_eq!(providers.qobuz_calls.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn missing_track_id_skips_the_qobuz_step() {
        let providers = Arc::new(FakeProviders::default());
        let (service, _dir) = service_with(providers.clone()).await;

        let mut no_id = request();
        no_id.track_id = None; // Qobuz source but no real id
        let response = service.get(no_id).await.unwrap();
        assert_eq!(response.outcome, LyricsOutcome::NotFound);
        assert_eq!(providers.qobuz_calls.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn empty_title_or_artist_is_an_error() {
        let providers = Arc::new(FakeProviders::default());
        let (service, _dir) = service_with(providers).await;

        let mut bad = request();
        bad.title = "   ".into();
        assert!(service.get(bad).await.is_err());

        let mut bad = request();
        bad.artist = String::new();
        assert!(service.get(bad).await.is_err());
    }

    #[tokio::test]
    async fn no_session_is_an_error() {
        let service = LyricsService::new(Arc::new(FakeProviders::default()));
        let err = service.get(request()).await.unwrap_err();
        assert!(err.contains("No active session"));
        assert!(service.cache_stats().await.is_err());
    }

    #[tokio::test]
    async fn teardown_drops_the_cache_handle() {
        let (service, _dir) = service_with(Arc::new(FakeProviders::default())).await;
        assert!(service.cache_stats().await.is_ok());
        service.teardown().await;
        assert!(service.cache_stats().await.is_err());
    }

    // ---------- dedupe (F6) ----------

    #[tokio::test]
    async fn concurrent_same_request_resolves_once() {
        let providers = Arc::new(FakeProviders {
            delay_ms: Some(50),
            ..Default::default()
        });
        providers
            .qobuz_queue
            .lock()
            .unwrap()
            .push_back(Ok(Some(wsync_document())));
        let dir = tempfile::tempdir().unwrap();
        let service = Arc::new(LyricsService::new(providers.clone()));
        service.init_at(dir.path()).await.unwrap();

        let (a, b) = tokio::join!(service.get(request()), service.get(request()));
        let (a, b) = (a.unwrap(), b.unwrap());
        assert_eq!(found(&a).payload.provider, LyricsProvider::Qobuz);
        assert_eq!(a, b); // both callers got the one shared resolution
        assert_eq!(providers.qobuz_calls.load(Ordering::SeqCst), 1);

        // The in-flight entry is gone afterwards (a later call re-resolves —
        // synced cache hit this time, still no second provider call).
        let c = service.get(request()).await.unwrap();
        assert!(found(&c).payload.cached);
        assert_eq!(providers.qobuz_calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn different_requests_do_not_dedupe_against_each_other() {
        let providers = Arc::new(FakeProviders {
            delay_ms: Some(20),
            ..Default::default()
        });
        {
            let mut q = providers.qobuz_queue.lock().unwrap();
            q.push_back(Ok(Some(wsync_document())));
            q.push_back(Ok(Some(wsync_document())));
        }
        let dir = tempfile::tempdir().unwrap();
        let service = Arc::new(LyricsService::new(providers.clone()));
        service.init_at(dir.path()).await.unwrap();

        let mut other = request();
        other.track_id = Some(200);
        other.title = "Other".into();
        let (a, b) = tokio::join!(service.get(request()), service.get(other));
        assert!(a.is_ok() && b.is_ok());
        assert_eq!(providers.qobuz_calls.load(Ordering::SeqCst), 2);
    }

    // ---------- Qobuzify (ZBQ) ----------

    fn qobuzify_syllable() -> QobuzifyOutcome {
        QobuzifyOutcome::Found(
            r#"{"Type":"Syllable","Content":[{"Type":"Vocal","Lead":{"StartTime":1.0,"EndTime":2.0,"Syllables":[
                {"Text":"qz","StartTime":1.0,"EndTime":1.5,"IsPartOfWord":false},
                {"Text":"line","StartTime":1.5,"EndTime":2.0,"IsPartOfWord":false}]}}]}"#
                .to_string(),
        )
    }

    fn qobuzify_static() -> QobuzifyOutcome {
        QobuzifyOutcome::Found(r#"{"Type":"Static","Lines":[{"Text":"sheet one"},{"Text":"sheet two"}]}"#.to_string())
    }

    #[tokio::test]
    async fn qobuzify_synced_wins_and_is_cached() {
        let providers = Arc::new(FakeProviders::default());
        providers.qobuzify_queue.lock().unwrap().push_back((0, qobuzify_syllable()));
        providers.lrclib_queue.lock().unwrap().push_back(Ok(Some(lrclib_synced())));
        let (service, _dir) = service_with(providers.clone()).await;

        let response = service.get(request()).await.unwrap();
        let result = found(&response);
        assert_eq!(result.payload.provider, LyricsProvider::Qobuzify);
        assert!(result.doc.synced);
        assert_eq!(result.doc.lines[0].text, "qz line");
        assert!(result.doc.native_json.as_deref().unwrap().contains("Syllable"), "raw object kept for Lyra");
        assert!(result.payload.synced_lrc.as_deref().unwrap().contains("qz line"));

        // From the cache the second time, still with the raw object.
        let again = service.get(request()).await.unwrap();
        let cached = found(&again);
        assert_eq!(cached.payload.provider, LyricsProvider::Qobuzify);
        assert!(cached.doc.native_json.is_some());
        assert_eq!(providers.qobuzify_calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn a_late_qobuzify_answer_replaces_the_fallback() {
        let providers = Arc::new(FakeProviders::default());
        // Slower than the chain plus the grace.
        providers.qobuzify_queue.lock().unwrap().push_back((3_200, qobuzify_syllable()));
        providers.lrclib_queue.lock().unwrap().push_back(Ok(Some(lrclib_synced())));
        let (service, _dir) = service_with(providers.clone()).await;
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        service.on_upgrade(move |response| {
            let _ = tx.send(response);
        });

        let response = service.get(request()).await.unwrap();
        assert_eq!(found(&response).payload.provider, LyricsProvider::Lrclib, "the fallback is served first");

        let upgraded = tokio::time::timeout(Duration::from_secs(5), rx.recv()).await.unwrap().unwrap();
        assert_eq!(upgraded.request_key, response.request_key);
        assert_eq!(found(&upgraded).payload.provider, LyricsProvider::Qobuzify);
        // And it replaced the cache entry.
        let again = service.get(request()).await.unwrap();
        assert_eq!(found(&again).payload.provider, LyricsProvider::Qobuzify);
    }

    #[tokio::test]
    async fn qobuzify_is_not_asked_for_translations_or_offline() {
        let providers = Arc::new(FakeProviders::default());
        providers.qobuzify_queue.lock().unwrap().push_back((0, qobuzify_syllable()));
        providers.qobuz_queue.lock().unwrap().push_back(Ok(Some(wsync_document_with_translation())));
        let (service, _dir) = service_with(providers.clone()).await;
        let mut translated = request();
        translated.language = Some("es".into());
        let response = service.get(translated).await.unwrap();
        assert_eq!(found(&response).payload.provider, LyricsProvider::Qobuz);
        let mut offline = request();
        offline.offline = true;
        let _ = service.get(offline).await.unwrap();
        assert_eq!(providers.qobuzify_calls.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn an_unsynced_qobuzify_sheet_is_only_the_last_resort() {
        let providers = Arc::new(FakeProviders::default());
        providers.qobuzify_queue.lock().unwrap().push_back((0, qobuzify_static()));
        providers.lrclib_queue.lock().unwrap().push_back(Ok(Some(lrclib_synced())));
        let (service, _dir) = service_with(providers.clone()).await;
        let response = service.get(request()).await.unwrap();
        assert_eq!(found(&response).payload.provider, LyricsProvider::Lrclib, "synced LRCLIB beats a plain sheet");

        let providers = Arc::new(FakeProviders::default());
        providers.qobuzify_queue.lock().unwrap().push_back((0, qobuzify_static()));
        let (service, _dir) = service_with(providers.clone()).await;
        let response = service.get(request()).await.unwrap();
        let result = found(&response);
        assert_eq!(result.payload.provider, LyricsProvider::Qobuzify);
        assert!(!result.doc.synced);
        assert_eq!(result.doc.plain_text(), "sheet one\nsheet two");
        assert_eq!(providers.ovh_calls.load(Ordering::SeqCst), 1, "ovh still had its turn first");
    }

    #[tokio::test]
    async fn qobuzify_is_asked_once_per_track_per_run() {
        let providers = Arc::new(FakeProviders::default());
        providers.lrclib_queue.lock().unwrap().push_back(Ok(Some(lrclib_synced())));
        let (service, _dir) = service_with(providers.clone()).await;
        let first = service.get(request()).await.unwrap();
        assert_eq!(found(&first).payload.provider, LyricsProvider::Lrclib);
        let again = service.get(request()).await.unwrap();
        assert_eq!(found(&again).payload.provider, LyricsProvider::Lrclib);
        assert_eq!(providers.qobuzify_calls.load(Ordering::SeqCst), 1);
    }

    // ---------- "Wrong lyrics?" (Qobuzify refetch) ----------

    fn qobuzify_with_source(source: &str, text: &str) -> String {
        format!(
            r#"{{"Type":"Syllable","zbqSource":"{source}","Content":[{{"Type":"Vocal","Lead":{{"StartTime":1.0,"EndTime":2.0,"Syllables":[
                {{"Text":"{text}","StartTime":1.0,"EndTime":2.0,"IsPartOfWord":false}}]}}}}]}}"#
        )
    }

    #[tokio::test]
    async fn wrong_lyrics_switches_caches_and_remembers_the_rejected_source() {
        let providers = Arc::new(FakeProviders::default());
        providers
            .qobuzify_queue
            .lock()
            .unwrap()
            .push_back((0, QobuzifyOutcome::Found(qobuzify_with_source("Ne Plus Ultra", "first"))));
        providers
            .refetch_queue
            .lock()
            .unwrap()
            .push_back(RefetchOutcome::Alternative(qobuzify_with_source("Coloratura", "second")));
        let (service, dir) = service_with(providers.clone()).await;

        let shown = service.get(request()).await.unwrap();
        assert_eq!(found(&shown).doc.lines[0].text, "first");

        let RefetchResult::Switched(switched) = service.refetch_qobuzify(request()).await.unwrap() else {
            panic!("expected another version");
        };
        assert_eq!(switched.request_key, shown.request_key);
        assert_eq!(found(&switched).doc.lines[0].text, "second");
        // The request named the sheet on screen and excluded it.
        let calls = providers.refetch_calls.lock().unwrap().clone();
        assert_eq!(calls, vec![(Some("Ne Plus Ultra".to_string()), vec!["Ne Plus Ultra".to_string()])]);
        // The pick replaced the cache entry.
        let again = service.get(request()).await.unwrap();
        assert_eq!(found(&again).doc.lines[0].text, "second");
        // And the rejection persists for the next session's requests.
        let saved = std::fs::read_to_string(dir.path().join("lyrics").join("qobuzify_rejected.json")).unwrap();
        assert!(saved.contains("Ne Plus Ultra"), "{saved}");
    }

    #[tokio::test]
    async fn rejected_sources_are_excluded_from_later_lookups() {
        let providers = Arc::new(FakeProviders::default());
        let (service, dir) = service_with(providers.clone()).await;
        let key = build_cache_key("Title", "Artist", Some(200));
        std::fs::write(
            dir.path().join("lyrics").join("qobuzify_rejected.json"),
            format!(r#"{{"v":1,"t":{{"{key}":{{"s":["Ne Plus Ultra"],"t":1}}}}}}"#),
        )
        .unwrap();
        // A new session reads the list.
        service.init_at(dir.path()).await.unwrap();
        let _ = service.get(request()).await.unwrap();
        assert_eq!(providers.qobuzify_excludes.lock().unwrap().clone(), vec![vec!["Ne Plus Ultra".to_string()]]);
    }

    #[tokio::test]
    async fn wrong_lyrics_reports_the_other_answers() {
        let providers = Arc::new(FakeProviders::default());
        {
            let mut queue = providers.refetch_queue.lock().unwrap();
            queue.push_back(RefetchOutcome::OnlyVersion);
            queue.push_back(RefetchOutcome::TryLater);
        }
        let (service, _dir) = service_with(providers.clone()).await;
        assert!(matches!(service.refetch_qobuzify(request()).await.unwrap(), RefetchResult::OnlyVersion));
        assert!(matches!(service.refetch_qobuzify(request()).await.unwrap(), RefetchResult::TryLater));
        assert!(matches!(service.refetch_qobuzify(request()).await.unwrap(), RefetchResult::Failed));
        let mut offline = request();
        offline.offline = true;
        assert!(matches!(service.refetch_qobuzify(offline).await.unwrap(), RefetchResult::Failed));
        // Nothing was cached for these, so no served codename went out.
        assert!(providers.refetch_calls.lock().unwrap().iter().all(|(served, _)| served.is_none()));
    }
}
