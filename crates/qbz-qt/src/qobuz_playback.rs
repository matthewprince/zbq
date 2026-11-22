//! Qobuz playback through the official player (`qobuz_engine.rs`).
//!
//! The app's queue stays the source of truth. When the queue reaches a Qobuz track,
//! [`start`] hands that one track to Qobuz's official web player, which fetches,
//! decrypts and plays it itself (see COMPLIANCE.md). While it plays, the playback
//! poll loop calls [`tick`] to mirror the official player's position/state into the
//! now-playing model and to learn when the track ended, so the queue can advance to
//! whatever comes next (Qobuz again, or a local/media-server track). Starting any
//! non-Qobuz track calls [`release`], which pauses the official player.
//!
//! A start that arrives before the official player can take it (the page is still
//! loading, or nobody is signed in on it) is kept as the pending start: the sign-in
//! page opens if needed, and the track starts once the player is ready and signed
//! in, unless something else was played (or the sign-in cancelled) meanwhile.
//!
//! Audio: with Firefox installed the official player runs in Firefox
//! (`firefox_engine.rs`) and Qobuz tracks play BIT-PERFECT: Firefox keeps each file's
//! own sample rate and PipeWire is set to it. The tracks the queue hands over play as
//! one continuous stream there, so a queued successor follows without a gap
//! ([`prefetch_successor`]); [`tick`] reports that stream's clock, the gapless edge
//! and the end. Without Firefox (or with `ZBQ_QOBUZ_AUDIO=web`) the official player
//! runs in Qt WebEngine and makes the sound itself; that is not bit-perfect (Chromium
//! resamples everything to one fixed rate and fades in each start).

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use qbz_app::shell::AppRuntime;
use qbz_core::LoggingAdapter;

/// The Qobuz track id the official player was asked to play (0 = it owns nothing).
static ACTIVE: AtomicU64 = AtomicU64::new(0);
/// A start position to apply once the official player has loaded `.0` (ms).
static PENDING_SEEK: Mutex<Option<(u64, u64)>> = Mutex::new(None);
/// Whether the official player was seen playing the active track (end detection
/// must not fire before the track ever started).
static SEEN_PLAYING: Mutex<Option<u64>> = Mutex::new(None);
/// A start waiting for the official player: (track id, start seconds).
static PENDING_START: Mutex<Option<(u64, u64)>> = Mutex::new(None);
/// Whether the task that runs [`PENDING_START`] is alive.
static WAITER_RUNNING: AtomicBool = AtomicBool::new(false);
/// Whether that task opened the sign-in page (and so must close it).
static WAITER_OPENED_LOGIN: AtomicBool = AtomicBool::new(false);
/// Firefox stream: when the active track was handed over (no-sound guard), whether
/// the user paused it, and the track whose end (or gapless edge) was reported.
static BEGUN_AT: Mutex<Option<Instant>> = Mutex::new(None);
static PAUSED: AtomicBool = AtomicBool::new(false);
static END_REPORTED: AtomicU64 = AtomicU64::new(0);
/// The track a gapless successor was last asked for (once per start).
static GAPLESS_ASKED: AtomicU64 = AtomicU64::new(0);

/// How long a pending start waits for the page to load, and for a sign-in.
const LOAD_WAIT: Duration = Duration::from_secs(60);
const SIGN_IN_WAIT: Duration = Duration::from_secs(600);

/// True while the official player owns the audible Qobuz track.
pub fn engine_owns_playback() -> bool {
    ACTIVE.load(Ordering::SeqCst) != 0
}

/// The active Qobuz track's position for the lyrics (ms) and whether it is
/// playing, while the official player owns playback (`None` otherwise). In
/// Firefox mode the clock is the stream, extrapolated since its last state
/// read (every 50 ms); in Qt WebEngine mode it is the page's own position.
pub fn lyrics_clock() -> Option<(f64, bool)> {
    let active = ACTIVE.load(Ordering::SeqCst);
    if active == 0 {
        return None;
    }
    if crate::firefox_engine::enabled() {
        let Some(stream) = crate::firefox_engine::shadow().filter(|s| s.track_u64() == Some(active)) else {
            return Some((0.0, false));
        };
        let mut position = stream.time_ms as f64;
        if stream.playing {
            if let Some(at) = stream.received_at {
                position += at.elapsed().as_secs_f64() * 1000.0;
            }
        }
        if stream.duration_ms > 0 {
            position = position.min(stream.duration_ms as f64);
        }
        return Some((position, stream.playing));
    }
    let state = crate::qobuz_engine::state()?;
    if state.track_id_u64() != Some(active) {
        return Some((0.0, false));
    }
    Some((state.position_ms as f64, state.playing))
}

/// Start `track_id` in the official player, `start_secs` into it. If the player
/// cannot take it yet, it becomes the pending start (see the module docs).
pub async fn start(
    runtime: &Arc<AppRuntime<LoggingAdapter>>,
    track_id: u64,
    start_secs: u64,
) -> Result<(), String> {
    let state = crate::qobuz_engine::state().unwrap_or_default();
    if state.ready && state.logged_in {
        *PENDING_START.lock().unwrap() = None;
        begin(runtime, track_id, start_secs);
        return Ok(());
    }
    // Nothing else may keep sounding while the track waits.
    let _ = runtime.core().stop();
    release();
    *PENDING_START.lock().unwrap() = Some((track_id, start_secs));
    log::info!(
        "[qobuz-playback] track {track_id} waits for the official player (ready={} signed_in={})",
        state.ready,
        state.logged_in
    );
    spawn_waiter(runtime.clone());
    Ok(())
}

/// Hand `track_id` to the official player now (it is ready and signed in).
fn begin(runtime: &Arc<AppRuntime<LoggingAdapter>>, track_id: u64, start_secs: u64) {
    // A local/media-server track may be playing natively: stop it first so the
    // two never sound at once.
    let _ = runtime.core().stop();
    ACTIVE.store(track_id, Ordering::SeqCst);
    *SEEN_PLAYING.lock().unwrap() = None;
    *PENDING_SEEK.lock().unwrap() = (start_secs > 0).then_some((track_id, start_secs * 1000));
    let mut already_fetching = false;
    if crate::firefox_engine::enabled() {
        // Firefox makes the sound on this machine: local playback to the UI.
        already_fetching = crate::firefox_engine::begin(track_id, start_secs * 1000);
        *BEGUN_AT.lock().unwrap() = Some(Instant::now());
        PAUSED.store(false, Ordering::SeqCst);
        END_REPORTED.store(0, Ordering::SeqCst);
        GAPLESS_ASKED.store(0, Ordering::SeqCst);
        crate::now_playing::set_remote(false, "");
    } else {
        crate::now_playing::set_remote(true, "Qobuz");
    }
    // A prefetched successor is already playing in the official player.
    if !already_fetching {
        crate::qobuz_engine::play(&[track_id], 0, "track", &track_id.to_string());
    }
    log::info!("[qobuz-playback] track {track_id} handed to the official player");
}

/// Run the pending start once the official player is ready and signed in,
/// opening its sign-in page when it is ready but signed out.
fn spawn_waiter(runtime: Arc<AppRuntime<LoggingAdapter>>) {
    if WAITER_RUNNING.swap(true, Ordering::SeqCst) {
        return;
    }
    crate::spawn(async move {
        let started = Instant::now();
        loop {
            tokio::time::sleep(Duration::from_millis(250)).await;
            let Some((track_id, start_secs)) = *PENDING_START.lock().unwrap() else {
                // Replaced by a native play (or a direct start), or the sign-in
                // was cancelled.
                close_login_if_ours();
                break;
            };
            let state = crate::qobuz_engine::state().unwrap_or_default();
            if state.ready && state.logged_in {
                *PENDING_START.lock().unwrap() = None;
                close_login_if_ours();
                begin(&runtime, track_id, start_secs);
                break;
            }
            if state.ready && !crate::qobuz_engine::login_open() {
                WAITER_OPENED_LOGIN.store(true, Ordering::SeqCst);
                crate::qobuz_engine::set_login_open(true);
            }
            let limit = if state.ready { SIGN_IN_WAIT } else { LOAD_WAIT };
            if started.elapsed() > limit {
                *PENDING_START.lock().unwrap() = None;
                close_login_if_ours();
                crate::toast_qt::error(qbz_i18n::t(if state.ready {
                    "Sign in on Qobuz's page to play Qobuz tracks."
                } else {
                    "Qobuz's player didn't load. Check your connection and try again."
                }));
                break;
            }
        }
        WAITER_RUNNING.store(false, Ordering::SeqCst);
    });
}

fn close_login_if_ours() {
    if WAITER_OPENED_LOGIN.swap(false, Ordering::SeqCst) {
        crate::qobuz_engine::set_login_open(false);
    }
}

/// The user cancelled the sign-in page: drop the pending start.
pub fn cancel_pending() {
    *PENDING_START.lock().unwrap() = None;
    WAITER_OPENED_LOGIN.store(false, Ordering::SeqCst);
}

/// A non-Qobuz track is about to play natively: pause the official player and
/// drop any pending start.
pub fn release() {
    *PENDING_START.lock().unwrap() = None;
    if ACTIVE.swap(0, Ordering::SeqCst) != 0 {
        if crate::firefox_engine::enabled() {
            crate::firefox_engine::stop_audio();
        }
        crate::qobuz_engine::pause();
        *PENDING_SEEK.lock().unwrap() = None;
        *SEEN_PLAYING.lock().unwrap() = None;
        crate::now_playing::set_remote(false, "");
        log::info!("[qobuz-playback] official player released");
    }
}

/// Transport while the official player owns playback. With Firefox both the stream
/// and the (muted) official player follow, so it keeps fetching around the same
/// position.
pub fn set_playing(playing: bool) {
    if crate::firefox_engine::enabled() {
        PAUSED.store(!playing, Ordering::SeqCst);
        crate::firefox_engine::set_paused(!playing);
    }
    if playing {
        crate::qobuz_engine::resume();
    } else {
        crate::qobuz_engine::pause();
    }
}
/// `track_id` is the gapless successor already queued in the Firefox stream: its
/// start adopts what is playing, so nothing may stop the stream first.
pub fn continues_in_stream(track_id: u64) -> bool {
    crate::firefox_engine::enabled() && crate::firefox_engine::is_successor(track_id)
}
/// Whether the active Qobuz track is meant to be playing (the play/pause toggle).
pub fn is_playing() -> bool {
    if crate::firefox_engine::enabled() {
        return !PAUSED.load(Ordering::SeqCst);
    }
    crate::qobuz_engine::state().map(|s| s.playing).unwrap_or(false)
}
/// The active track's length, as the stream (or the official player) knows it.
pub fn duration_secs() -> Option<f64> {
    let active = ACTIVE.load(Ordering::SeqCst);
    crate::firefox_engine::shadow()
        .filter(|s| s.track_u64() == Some(active) && s.duration_ms > 0)
        .map(|s| s.duration_ms)
        .or_else(|| crate::qobuz_engine::state().filter(|s| s.duration_ms > 0).map(|s| s.duration_ms))
        .map(|ms| ms as f64 / 1000.0)
}
/// The volume control. With Firefox it is the output device's volume (the stream
/// stays at 100%, see `firefox_engine::set_volume`).
pub fn set_volume(fraction: f32) {
    if crate::firefox_engine::enabled() {
        crate::firefox_engine::set_volume(fraction);
    } else {
        crate::qobuz_engine::set_volume(fraction);
    }
}
/// Mute and unmute (`restore` is the level to go back to without Firefox).
pub fn set_muted(muted: bool, restore: f32) {
    if crate::firefox_engine::enabled() {
        crate::firefox_engine::set_muted(muted);
    } else {
        crate::qobuz_engine::set_volume(if muted { 0.0 } else { restore });
    }
}
/// Gapless (the poll loop's gapless request near the end of the track): once the
/// current track's audio has fully arrived in the stream, start `next_id` in the
/// official player as the successor, so its audio is queued behind the current one.
pub async fn prefetch_successor(current_id: u64, next_id: u64, quality_id: u32) {
    for _ in 0..40 {
        match crate::firefox_engine::shadow() {
            Some(s) if s.received(current_id) => break,
            Some(s) if s.holds(current_id) => tokio::time::sleep(Duration::from_millis(250)).await,
            _ => return,
        }
    }
    if !crate::firefox_engine::shadow().is_some_and(|s| s.received(current_id)) {
        log::info!("[qobuz-playback] {current_id} still arriving; {next_id} starts normally");
        return;
    }
    if ACTIVE.load(Ordering::SeqCst) != current_id {
        return;
    }
    crate::firefox_engine::queue_next(next_id);
    crate::qobuz_engine::set_max_format(quality_id);
    crate::qobuz_engine::play(&[next_id], 0, "track", &next_id.to_string());
    log::info!("[qobuz-playback] prefetching {next_id} for a gapless transition");
}

/// Seek to `frac` of the track.
pub fn seek_frac(frac: f32) {
    let frac = f64::from(frac.clamp(0.0, 1.0));
    let official_ms = crate::qobuz_engine::state().map(|s| s.duration_ms).unwrap_or(0);
    if crate::firefox_engine::enabled() {
        // The official player reloads the track at the new position (applied once its
        // new load shows up in the stream, see `tick`) and the stream starts there.
        let active = ACTIVE.load(Ordering::SeqCst);
        if let (true, Some(duration)) = (active != 0, duration_secs()) {
            let ms = (duration * frac * 1000.0) as u64;
            // Inside what the stream already holds: move there at once. Forward, the
            // official player follows (it holds what lies ahead); backward it is left
            // alone, since it may have let go of what is behind it, and seeking it
            // there would make it load again and the stream drop its copy.
            let heard_ms = crate::firefox_engine::shadow().map(|s| s.time_ms).unwrap_or(0);
            if crate::firefox_engine::seek_in_stream(active, ms) {
                if ms > heard_ms {
                    crate::qobuz_engine::seek_ms(ms);
                }
                END_REPORTED.store(0, Ordering::SeqCst);
                return;
            }
            // A seek while the track is still loading (a second click, say): only
            // where it starts changes. Loading it again would leave the stream
            // waiting on the official player, which does not reload what it holds.
            let loading = BEGUN_AT.lock().unwrap().is_some_and(|t| t.elapsed() < SILENCE_LIMIT)
                && crate::firefox_engine::shadow().map_or(true, |s| !s.started);
            if loading {
                crate::firefox_engine::retarget(ms);
                let mut pending = PENDING_SEEK.lock().unwrap();
                if pending.is_some_and(|(id, _)| id == active) {
                    *pending = Some((active, ms));
                } else {
                    drop(pending);
                    crate::qobuz_engine::seek_ms(ms);
                }
                return;
            }
            crate::firefox_engine::begin(active, ms);
            END_REPORTED.store(0, Ordering::SeqCst);
            GAPLESS_ASKED.store(0, Ordering::SeqCst);
            *BEGUN_AT.lock().unwrap() = Some(Instant::now());
            *PENDING_SEEK.lock().unwrap() = (ms > 0).then_some((active, ms));
            crate::qobuz_engine::play(&[active], 0, "track", &active.to_string());
            if PAUSED.load(Ordering::SeqCst) {
                crate::firefox_engine::set_paused(true);
            }
        }
        return;
    }
    if official_ms > 0 {
        crate::qobuz_engine::seek_ms((official_ms as f64 * frac) as u64);
    }
}

/// One poll-loop observation of the official player.
pub struct Tick {
    pub track_id: u64,
    pub position_ms: u64,
    pub duration_ms: u64,
    pub playing: bool,
    pub volume: Option<f32>,
    pub muted: Option<bool>,
    /// The active track finished: advance the queue.
    pub ended: bool,
    /// Firefox stream: the track nears its end, time to queue a gapless successor.
    pub gapless_due: bool,
}

/// No sound from the stream this long after a start means nothing is arriving (a
/// changed web player, or a load that failed).
const SILENCE_LIMIT: Duration = Duration::from_secs(30);
/// How long before the end the successor is asked for (as the native engine does).
const GAPLESS_LEAD_MS: u64 = 10_000;

/// Called from the playback poll loop. `None` when the official player owns nothing.
pub fn tick(runtime: &Arc<AppRuntime<LoggingAdapter>>) -> Option<Tick> {
    let active = ACTIVE.load(Ordering::SeqCst);
    if active == 0 {
        return None;
    }
    let state = crate::qobuz_engine::state()?;
    let on_track = state.track_id_u64() == Some(active);
    // With Firefox a start position waits until the official player's new load of
    // the track has reached the stream (the old load may still be showing).
    let loaded = !crate::firefox_engine::enabled()
        || crate::firefox_engine::shadow().is_some_and(|s| s.holds(active));
    if on_track && state.duration_ms > 0 && loaded {
        if let Some((id, ms)) = PENDING_SEEK.lock().unwrap().take() {
            if id == active {
                crate::qobuz_engine::seek_ms(ms);
            }
        }
    }
    if crate::firefox_engine::enabled() {
        let _ = runtime;
        return firefox_tick(active, &state);
    }
    let mut seen = SEEN_PLAYING.lock().unwrap();
    if on_track && state.playing {
        *seen = Some(active);
    }
    // End of track: the official player stopped on our track after playing it, at
    // (or within a second of) its end. A pause mid-track is not an end.
    let near_end = state.duration_ms > 0 && state.position_ms + 1000 >= state.duration_ms;
    let stopped_on_track =
        on_track && !state.playing && (state.playing_state == "stopped" || near_end);
    // The official player moved on to a different track after ours played (its own
    // next, or anything done inside the page): ours is over either way.
    let moved_on = !on_track && !state.track_id.is_empty();
    let ended = *seen == Some(active) && (stopped_on_track || moved_on);
    if ended {
        *seen = None;
    }
    Some(Tick {
        track_id: active,
        position_ms: if on_track { state.position_ms } else { 0 },
        duration_ms: if on_track { state.duration_ms } else { 0 },
        playing: on_track && state.playing,
        volume: state.volume.map(|v| v as f32),
        muted: Some(state.muted),
        ended,
        gapless_due: false,
    })
}

/// Firefox: the stream is the clock. It reports the gapless edge (the successor is
/// now heard: the active track ended, the queue moves on and `begin` adopts the
/// successor without a restart), the natural end, and when to queue a successor.
fn firefox_tick(active: u64, page: &crate::qobuz_engine::EngineState) -> Option<Tick> {
    let start_ms = PENDING_SEEK.lock().unwrap().filter(|(id, _)| *id == active).map(|(_, ms)| ms).unwrap_or(0);
    let page_duration = if page.track_id_u64() == Some(active) { page.duration_ms } else { 0 };
    let waiting = Tick {
        track_id: active,
        position_ms: start_ms,
        duration_ms: page_duration,
        playing: false,
        // The volume control shows the output device's volume.
        volume: crate::firefox_engine::device_volume(),
        muted: None,
        ended: false,
        gapless_due: false,
    };
    let Some(stream) = crate::firefox_engine::shadow() else {
        return Some(waiting);
    };
    let current = stream.track_u64();
    if let Some(now) = current.filter(|now| *now != active && stream.holds(active)) {
        if END_REPORTED.swap(active, Ordering::SeqCst) != active {
            log::info!("[qobuz-playback] gapless: {active} -> {now}");
            return Some(Tick { position_ms: page_duration, playing: true, ended: true, ..waiting });
        }
        return Some(waiting);
    }
    let on = current == Some(active);
    if on && stream.rate > 0 {
        // Not bit-perfect when the output is locked (PipeWire may resample) or the
        // device has no such rate (PipeWire must resample).
        let resampled = crate::settings_qt::audio_settings().skip_sink_switch
            || crate::firefox_engine::device_plays(stream.rate) == Some(false);
        crate::now_playing::set_effective_stream(stream.rate, stream.bits, if resampled { "disabled" } else { "plugin" });
    }
    if stream.ended && (on || current.is_none()) {
        if END_REPORTED.swap(active, Ordering::SeqCst) != active {
            return Some(Tick { position_ms: stream.duration_ms, duration_ms: stream.duration_ms, ended: true, ..waiting });
        }
        return Some(Tick { position_ms: stream.duration_ms, duration_ms: stream.duration_ms, ..waiting });
    }
    if !stream.started && !PAUSED.load(Ordering::SeqCst) {
        let silent = BEGUN_AT.lock().unwrap().map(|t| t.elapsed()).unwrap_or_default();
        if silent > SILENCE_LIMIT {
            log::warn!("[qobuz-playback] no sound from the stream for track {active} after {} s", silent.as_secs());
            release();
            crate::toast_qt::error(qbz_i18n::t(
                "Qobuz's player didn't start this track. Try it again, or start ZBQ with ZBQ_QOBUZ_AUDIO=web to use Qobuz's own (not bit-perfect) audio.",
            ));
            return None;
        }
    }
    if !on {
        return Some(waiting);
    }
    Some(Tick {
        track_id: active,
        position_ms: stream.time_ms,
        duration_ms: if stream.duration_ms > 0 { stream.duration_ms } else { page_duration },
        playing: stream.playing,
        volume: crate::firefox_engine::device_volume(),
        muted: None,
        ended: false,
        gapless_due: stream.duration_ms > 0
            && stream.time_ms + GAPLESS_LEAD_MS >= stream.duration_ms
            && GAPLESS_ASKED.swap(active, Ordering::SeqCst) != active,
    })
}
