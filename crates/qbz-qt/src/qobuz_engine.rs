//! The official-player engine: Qobuz's own web player (play.qobuz.com), driven
//! through its own action creators by `engine/qobuz_engine_bridge.js`. It runs in a
//! hidden Firefox when Firefox is installed (`firefox_engine.rs`, bit-perfect), else
//! in a WebEngineView (`qml/engine/QobuzEngine.qml`). Both hosts take the same
//! commands and answer with the same state.
//!
//! Why this exists: upstream QBZ fetched, signed and decrypted Qobuz streams itself,
//! which is circumvention of Qobuz's access controls (see COMPLIANCE.md). Here Qobuz's
//! code does all of that, exactly as in a browser; this module only asks it to play
//! track ids, mirrors its state, and receives the user's own token after the user signs
//! in on Qobuz's own page. It never sees a stream URL, a key, a secret or any audio.
//!
//! Threading: commands are queued onto the Qt thread through the bridge's hop
//! (`qobuz_engine_bridge::ui`); state arrives from QML's poll on the Qt thread.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, RwLock};

use serde::Deserialize;
use tokio::sync::watch;

/// One state snapshot from the page (`window.__zbq.state()`).
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EngineState {
    #[serde(default)]
    pub ready: bool,
    #[serde(default)]
    pub logged_in: bool,
    #[serde(default)]
    pub token: String,
    #[serde(default)]
    pub playing: bool,
    #[serde(default)]
    pub playing_state: String,
    #[serde(default)]
    pub track_id: String,
    #[serde(default)]
    pub duration_ms: u64,
    #[serde(default)]
    pub position_ms: u64,
    #[serde(default)]
    pub volume: Option<f64>,
    #[serde(default)]
    pub muted: bool,
    /// What the bridge's discovery found (store, action modules, module count).
    #[serde(default)]
    pub found: Option<serde_json::Value>,
    /// An exception the bridge caught while reading the page's state.
    #[serde(default)]
    pub error: Option<String>,
}

impl EngineState {
    pub fn track_id_u64(&self) -> Option<u64> {
        self.track_id.parse::<u64>().ok()
    }
}

static STATE: RwLock<Option<EngineState>> = RwLock::new(None);
/// Mirrors `QbzQobuzEngine.loginOpen` (set only through [`set_login_open`]).
static LOGIN_OPEN: AtomicBool = AtomicBool::new(false);
/// The token the page held when the app signed it out. Until the page has
/// reported itself signed out once, a report still carrying this token is the
/// log-out in flight, not a sign-in.
static SIGNED_OUT_TOKEN: Mutex<Option<String>> = Mutex::new(None);
static TOKEN_TX: Mutex<Option<watch::Sender<Option<String>>>> = Mutex::new(None);

/// The latest snapshot, `None` until the page has answered once.
pub fn state() -> Option<EngineState> {
    STATE.read().ok().and_then(|s| s.clone())
}

/// Called by the bridge on every poll answer.
pub(crate) fn on_state_json(json: &str) {
    let Ok(mut parsed) = serde_json::from_str::<EngineState>(json) else {
        return;
    };
    {
        let mut signed_out = SIGNED_OUT_TOKEN.lock().unwrap();
        if let Some(old) = signed_out.as_ref() {
            if parsed.logged_in && parsed.token == *old {
                parsed.logged_in = false;
                parsed.token.clear();
            } else {
                *signed_out = None;
            }
        }
    }
    let token = (parsed.logged_in && !parsed.token.is_empty()).then(|| parsed.token.clone());
    if let Ok(mut slot) = STATE.write() {
        log_transition(slot.as_ref(), &parsed);
        *slot = Some(parsed);
    }
    publish_token(token);
}

/// Keep the token watch equal to what the page holds now (None = signed out),
/// waking subscribers only on a change.
fn publish_token(token: Option<String>) {
    if let Ok(guard) = TOKEN_TX.lock() {
        if let Some(tx) = guard.as_ref() {
            tx.send_if_modified(|current| {
                if *current == token {
                    false
                } else {
                    *current = token;
                    true
                }
            });
        }
    }
}

/// One log line whenever readiness, sign-in or discovery changes, so a Qobuz
/// web-player update that breaks the bridge is visible in the log. Never logs
/// the token.
fn log_transition(old: Option<&EngineState>, new: &EngineState) {
    let changed = match old {
        None => true,
        Some(o) => {
            o.ready != new.ready
                || o.logged_in != new.logged_in
                || o.found != new.found
                || o.error != new.error
                || o.playing_state != new.playing_state
                || o.track_id != new.track_id
        }
    };
    if changed {
        log::info!(
            "[qobuz-engine] ready={} signed_in={} track={} state={} found={} error={}",
            new.ready,
            new.logged_in,
            if new.track_id.is_empty() { "-" } else { new.track_id.as_str() },
            if new.playing_state.is_empty() { "-" } else { new.playing_state.as_str() },
            new.found.as_ref().map(|f| f.to_string()).unwrap_or_default(),
            new.error.as_deref().unwrap_or("none"),
        );
    }
}

/// Subscribe to the user's token as the official page reports it. The first value is
/// whatever the page already holds (a still-valid earlier sign-in counts).
pub(crate) fn token_watch() -> watch::Receiver<Option<String>> {
    let mut guard = TOKEN_TX.lock().unwrap();
    if let Some(tx) = guard.as_ref() {
        return tx.subscribe();
    }
    let current = state().and_then(|s| (s.logged_in && !s.token.is_empty()).then_some(s.token));
    let (tx, rx) = watch::channel(current);
    *guard = Some(tx);
    rx
}

// --- commands -------------------------------------------------------------------

fn command(name: &'static str, args: serde_json::Value) {
    let payload = args.to_string();
    if crate::firefox_engine::enabled() {
        crate::firefox_engine::page_call(name, &payload);
        return;
    }
    crate::qobuz_engine_bridge::ui(move |mut b| {
        b.as_mut().command(
            cxx_qt_lib::QString::from(name),
            cxx_qt_lib::QString::from(payload.as_str()),
        );
    });
}

/// Replace the official queue with `track_ids` and start at `index`. `origin_type` /
/// `origin_id` are the player's own origin tag (album, playlist, track, ...).
pub fn play(track_ids: &[u64], index: usize, origin_type: &str, origin_id: &str) {
    command(
        "play",
        serde_json::json!([track_ids, index, origin_type, origin_id]),
    );
}
pub fn pause() {
    command("pause", serde_json::json!([]));
}
pub fn resume() {
    command("resume", serde_json::json!([]));
}
pub fn seek_ms(ms: u64) {
    command("seek", serde_json::json!([ms]));
}
pub fn set_volume(fraction: f32) {
    command("setVolume", serde_json::json!([fraction.clamp(0.0, 1.0)]));
}
/// The official quality ceiling: 5 MP3, 6 CD, 7 Hi-Res up to 96 kHz, 27 Hi-Res up to 192.
pub fn set_max_format(format_id: u32) {
    command("setMaxFormat", serde_json::json!([format_id]));
}

/// Sign out of the official player (its own log-out), so the next sign-in starts
/// signed out. Called when the user logs out of the app.
pub fn sign_out() {
    let held = state().filter(|s| s.logged_in && !s.token.is_empty()).map(|s| s.token);
    if let Some(token) = held {
        *SIGNED_OUT_TOKEN.lock().unwrap() = Some(token);
    }
    if let Ok(mut slot) = STATE.write() {
        if let Some(s) = slot.as_mut() {
            s.logged_in = false;
            s.token.clear();
        }
    }
    publish_token(None);
    command("signOut", serde_json::json!([]));
}

/// Whether the official page is currently shown full-size for sign-in.
pub fn login_open() -> bool {
    LOGIN_OPEN.load(Ordering::SeqCst)
}

/// Show (or hide) the official page full-size so the user can sign in on it (with
/// the Firefox host: in a Firefox window, while the player is signed out).
pub fn set_login_open(open: bool) {
    LOGIN_OPEN.store(open, Ordering::SeqCst);
    crate::firefox_engine::set_login_open(open);
    crate::qobuz_engine_bridge::ui(move |mut b| {
        b.as_mut().set_login_open(open);
    });
}
