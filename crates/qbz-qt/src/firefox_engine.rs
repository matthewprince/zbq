//! Qobuz's official web player in Firefox: ZBQ's Qobuz engine (`ZBQ_QOBUZ_AUDIO=web`
//! keeps the Qt WebEngine host instead).
//!
//! Why Firefox: bit-perfect Qobuz playback without touching Qobuz's protection.
//! Firefox opens its audio output at each file's own sample rate and passes the
//! samples through unchanged; Chromium (Qt WebEngine) resamples everything to one
//! fixed rate and fades in every start. With PipeWire set to the track's rate, Qobuz's
//! own player in Firefox plays every sample exactly (measured, see COMPLIANCE.md).
//!
//! What runs: a hidden (headless) Firefox with ZBQ's own profile on play.qobuz.com,
//! driven over WebDriver BiDi (`bidi.rs`). The page gets the same scripts as the Qt
//! WebEngine host (`qobuz_engine_early.js`, `qobuz_engine_bridge.js`) plus
//! `qobuz_engine_shadow.js`, which plays the tracks the app queues as one continuous
//! stream (gapless, nothing cut at track ends). Qobuz's code does all fetching,
//! signing and decrypting, as in any browser; the audio never leaves Firefox. With no
//! Firefox installed, Firefox ESR is downloaded from Mozilla (`firefox_provision.rs`).
//!
//! The app talks to it like to the Qt WebEngine host: `qobuz_engine` commands
//! ([`page_call`]) and state answers (`qobuz_engine::on_state_json`). Playback
//! (`qobuz_playback.rs`) also drives the stream ([`begin`], [`queue_next`], [`shadow`],
//! ...). On the PipeWire side (`firefox_output.rs`):
//! * Before a run of tracks starts, the page asks for its sample rate; the host sets
//!   PipeWire to it (`clock.force-rate`, as the native PipeWire backend does), waits
//!   until the device runs at it, and answers. A rate the device lacks is never set
//!   (some devices reset when asked for one): as in the native backend, the nearest
//!   rate it has is, and PipeWire resamples to it.
//! * Pause is Firefox's own (it skips the few milliseconds of audio already handed
//!   to PipeWire). Pausing Firefox's stream inside PipeWire kept every sample, but it
//!   is the last thing that preceded each reset of a USB headset, so it is out until
//!   it is cleared.
//! * The app's volume is the output device's own volume, so the stream stays at 100%
//!   (bit-perfect wherever the device has a hardware volume control); mute is the
//!   stream's.
//! * The output device is the one chosen in ZBQ's audio settings, and the quality
//!   asked of Qobuz never exceeds what that device plays without resampling.
//!
//! Sign-in: a normal Firefox window with the same profile on Qobuz's own page (no
//! automation on the login page). Once the session is in the profile, the window
//! closes and the hidden player starts again, signed in.

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock, RwLock};
use std::time::{Duration, Instant};

use serde::Deserialize;
use serde_json::{json, Value};
use tokio::process::{Child, Command};
use tokio::sync::mpsc;

use crate::bidi::Bidi;
use crate::firefox_output as out;

const PLAYER_URL: &str = "https://play.qobuz.com/";
const SIGN_IN_LIMIT: Duration = Duration::from_secs(15 * 60);

/// The stream player's state (`window.__zbqShadow.state()`).
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct ShadowState {
    pub gen: u64,
    /// The track being heard (or about to be), "" when nothing is held.
    pub track: String,
    pub time_ms: u64,
    pub duration_ms: u64,
    pub playing: bool,
    pub started: bool,
    /// Everything queued has played and nothing else is held.
    pub ended: bool,
    pub need_rate: u32,
    pub rate: u32,
    pub bits: u32,
    pub tracks: Vec<ShadowTrack>,
    /// The stretch of the current track the stream holds around the play
    /// position, in track milliseconds (both 0 when nothing is held).
    pub held_from_ms: u64,
    pub held_to_ms: u64,
    pub log: Vec<String>,
    /// When the host read this state (the lyrics clock extrapolates from it).
    #[serde(skip)]
    pub received_at: Option<Instant>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct ShadowTrack {
    pub tid: String,
    /// All of the track's audio has arrived.
    pub eos: bool,
    pub rate: u32,
}

impl ShadowState {
    pub fn track_u64(&self) -> Option<u64> {
        self.track.parse().ok()
    }
    pub fn holds(&self, id: u64) -> bool {
        let id = id.to_string();
        self.tracks.iter().any(|t| t.tid == id)
    }
    pub fn received(&self, id: u64) -> bool {
        let id = id.to_string();
        self.tracks.iter().any(|t| t.tid == id && t.eos)
    }
}

/// What the host does, in order, on Firefox.
enum Cmd {
    Js(String),
    /// The quality ceiling the app asks for (capped to the output device).
    MaxFormat(u32),
    /// Pause (true) or resume Firefox's stream.
    Pause(bool),
    /// The output device's volume.
    DeviceVolume(f32),
    /// Move within what the stream holds of the current track (ms).
    SeekInStream(u64),
    /// A new start position for a stream that has not started yet (ms).
    Retarget(u64),
}

static ENABLED: OnceLock<bool> = OnceLock::new();
/// The Firefox in use: installed, or downloaded (`firefox_provision`).
static BINARY: Mutex<Option<PathBuf>> = Mutex::new(None);
static STARTED: AtomicBool = AtomicBool::new(false);
static SHUTDOWN: AtomicBool = AtomicBool::new(false);
static COMMANDS: OnceLock<mpsc::UnboundedSender<Cmd>> = OnceLock::new();
static SHADOW: RwLock<Option<ShadowState>> = RwLock::new(None);
/// Bumped by every [`begin`] that starts over: older stream answers are ignored.
static GEN: AtomicU64 = AtomicU64::new(0);
static LOGIN_WANTED: AtomicBool = AtomicBool::new(false);
/// The successor queued behind the current track ([`queue_next`]).
static SUCCESSOR: Mutex<Option<u64>> = Mutex::new(None);
/// Re-sent after Firefox (re)starts: the quality ceiling asked for, the stream volume.
static STICKY: Mutex<(Option<u32>, Option<f32>)> = Mutex::new((None, None));
/// The PipeWire rate this engine set (0 = none).
static FORCED_RATE: AtomicU32 = AtomicU32::new(0);
static CHILD_PID: AtomicU32 = AtomicU32::new(0);
/// The output device's volume as last read or set (for the app's volume control).
static DEVICE_VOLUME: Mutex<Option<f32>> = Mutex::new(None);
/// The sink Firefox was started on (None = the system default).
static LAUNCH_SINK: Mutex<Option<String>> = Mutex::new(None);
/// What the output sink plays without resampling: (sink, rates).
static DEVICE_FORMAT: Mutex<Option<(String, Option<out::DeviceRates>)>> = Mutex::new(None);

/// Firefox mode: on unless `ZBQ_QOBUZ_AUDIO=web`, or when no Firefox is installed and
/// none can be downloaded (Mozilla's builds are fetched for Linux x86-64).
pub fn enabled() -> bool {
    *ENABLED.get_or_init(|| {
        if std::env::var("ZBQ_QOBUZ_AUDIO").is_ok_and(|v| v == "web") {
            return false;
        }
        match find_firefox() {
            Some(path) => {
                log::info!("[firefox-engine] Qobuz plays through Firefox at {}", path.display());
                *BINARY.lock().unwrap() = Some(path);
                true
            }
            None if cfg!(all(target_os = "linux", target_arch = "x86_64")) => {
                log::info!("[firefox-engine] no Firefox installed: Firefox ESR will be downloaded from Mozilla");
                true
            }
            None => {
                log::warn!("[firefox-engine] Firefox not found; Qobuz uses the Qt WebEngine player (not bit-perfect)");
                false
            }
        }
    })
}

fn find_firefox() -> Option<PathBuf> {
    if let Some(path) = std::env::var_os("ZBQ_FIREFOX").map(PathBuf::from) {
        return path.is_file().then_some(path);
    }
    let path_dirs = std::env::var_os("PATH").map(|p| std::env::split_paths(&p).collect::<Vec<_>>()).unwrap_or_default();
    for name in ["firefox", "firefox-esr"] {
        for dir in &path_dirs {
            let candidate = dir.join(name);
            // A snap's Firefox cannot use a profile under ~/.local/share.
            if candidate.is_file() && !is_snap_firefox(&candidate) {
                return Some(candidate);
            }
        }
    }
    ["/opt/firefox/firefox", "/usr/lib/firefox/firefox", "/usr/lib64/firefox/firefox", "/usr/lib/firefox-esr/firefox-esr"]
        .iter()
        .map(PathBuf::from)
        .find(|p| p.is_file())
        .or_else(|| crate::firefox_provision::local_binary().filter(|p| p.is_file()))
}

/// A snap's Firefox, or the script Ubuntu installs as /usr/bin/firefox to start it.
fn is_snap_firefox(path: &Path) -> bool {
    if path.starts_with("/snap") || std::fs::canonicalize(path).is_ok_and(|p| p.starts_with("/snap")) {
        return true;
    }
    use std::io::Read;
    let mut head = [0u8; 4096];
    let n = std::fs::File::open(path).and_then(|mut f| f.read(&mut head)).unwrap_or(0);
    head.starts_with(b"#!") && String::from_utf8_lossy(&head[..n]).contains("/snap/")
}

/// The Firefox to run: the one found at startup, else a downloaded Firefox ESR.
async fn firefox_binary() -> Result<PathBuf, String> {
    if let Some(path) = BINARY.lock().unwrap().clone().filter(|p| p.is_file()) {
        return Ok(path);
    }
    let path = crate::firefox_provision::provision().await?;
    *BINARY.lock().unwrap() = Some(path.clone());
    Ok(path)
}

fn profile_dir() -> Result<PathBuf, String> {
    let dir = dirs::data_dir().ok_or("no data directory")?.join("zbq").join("qobuz-firefox");
    std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700));
    }
    // A quiet profile: no first-run pages, telemetry or updates; autoplay allowed
    // (the app starts playback); no resampling of the audio output.
    let prefs = [
        ("media.autoplay.default", "0"),
        ("media.autoplay.blocking_policy", "0"),
        ("media.resampling.enabled", "false"),
        ("browser.shell.checkDefaultBrowser", "false"),
        ("browser.startup.homepage_override.mstone", "\"ignore\""),
        ("startup.homepage_welcome_url", "\"\""),
        ("startup.homepage_welcome_url.additional", "\"\""),
        ("browser.aboutwelcome.enabled", "false"),
        ("datareporting.policy.dataSubmissionEnabled", "false"),
        ("datareporting.healthreport.uploadEnabled", "false"),
        ("toolkit.telemetry.reportingpolicy.firstRun", "false"),
        ("app.update.auto", "false"),
        ("app.update.disabledForTesting", "true"),
        ("browser.sessionstore.resume_from_crash", "false"),
        ("toolkit.startup.max_resumed_crashes", "-1"),
        ("browser.tabs.warnOnClose", "false"),
    ];
    let body: String = prefs.iter().map(|(k, v)| format!("user_pref(\"{k}\", {v});\n")).collect();
    std::fs::write(dir.join("user.js"), body).map_err(|e| e.to_string())?;
    Ok(dir)
}

/// Start the engine (once). Called from the engine bridge's boot.
pub fn start() {
    if !enabled() || STARTED.swap(true, Ordering::SeqCst) {
        return;
    }
    let (tx, rx) = mpsc::unbounded_channel();
    let _ = COMMANDS.set(tx);
    crate::spawn(host(rx));
}

/// Stop Firefox and undo the PipeWire rate (app exit).
pub fn shutdown() {
    SHUTDOWN.store(true, Ordering::SeqCst);
    let pid = CHILD_PID.swap(0, Ordering::SeqCst);
    if pid != 0 {
        unsafe {
            libc::kill(pid as i32, libc::SIGTERM);
        }
    }
    if FORCED_RATE.swap(0, Ordering::SeqCst) != 0 {
        qbz_audio::pipewire_backend::PipeWireBackend::release_forced_rate();
    }
}

fn send(cmd: Cmd) {
    if let Some(tx) = COMMANDS.get() {
        let _ = tx.send(cmd);
    }
}

/// `window.__zbq[name](...args)` in the official player (`qobuz_engine::command`).
pub fn page_call(name: &str, args_json: &str) {
    if name == "setMaxFormat" {
        if let Some(format) = serde_json::from_str::<Vec<u32>>(args_json).ok().and_then(|a| a.first().copied()) {
            STICKY.lock().unwrap().0 = Some(format);
            send(Cmd::MaxFormat(format));
            return;
        }
    }
    send(Cmd::Js(format!(
        "window.__zbq && window.__zbq[{n}] && window.__zbq[{n}].apply(null, {args_json})",
        n = serde_json::to_string(name).unwrap_or_default()
    )));
}

fn shadow_call(name: &str, args: Value) {
    send(Cmd::Js(format!(
        "window.__zbqShadow && window.__zbqShadow[{n}].apply(null, {args})",
        n = serde_json::to_string(name).unwrap_or_default()
    )));
}

/// Take `track_id` into the stream, `start_ms` in. Returns true when it is the
/// successor already queued behind the current track (it plays on by itself, so
/// the official player must not be asked to start it again).
pub fn begin(track_id: u64, start_ms: u64) -> bool {
    let mut successor = SUCCESSOR.lock().unwrap();
    if *successor == Some(track_id) && start_ms == 0 {
        *successor = None;
        log::info!("[firefox-engine] {track_id} continues in the running stream");
        return true;
    }
    *successor = None;
    let gen = GEN.fetch_add(1, Ordering::SeqCst) + 1;
    *SHADOW.write().unwrap() = None;
    shadow_call("reset", json!([track_id.to_string(), start_ms, gen]));
    false
}

/// Seek inside what the stream already holds of `track_id`, without a reload. False
/// when that position is not held (a second apart from either end): the caller
/// reloads instead.
pub fn seek_in_stream(track_id: u64, ms: u64) -> bool {
    let Some(state) = shadow() else { return false };
    let holds_it = state.started
        && state.track_u64() == Some(track_id)
        && state.held_to_ms > state.held_from_ms
        && ms >= state.held_from_ms
        && ms + 1_000 <= state.held_to_ms;
    if holds_it {
        send(Cmd::SeekInStream(ms));
    }
    holds_it
}

/// The stream has not started yet (a seek during a load): begin at `ms` instead.
pub fn retarget(ms: u64) {
    send(Cmd::Retarget(ms));
}

/// `track_id` follows the current track in the same stream (gapless). The official
/// player is asked to start it separately.
pub fn queue_next(track_id: u64) {
    *SUCCESSOR.lock().unwrap() = Some(track_id);
    shadow_call("queueNext", json!([track_id.to_string()]));
}

pub fn is_successor(track_id: u64) -> bool {
    *SUCCESSOR.lock().unwrap() == Some(track_id)
}

pub fn set_paused(paused: bool) {
    send(Cmd::Pause(paused));
}

/// The app's volume control: the output device's own volume, so the stream stays at
/// 100%. 0 mutes the stream instead.
pub fn set_volume(fraction: f32) {
    if fraction <= 0.0 {
        set_muted(true);
        return;
    }
    set_muted(false);
    send(Cmd::DeviceVolume(fraction.clamp(0.0, 1.0)));
}

/// Mute or unmute the stream; the device's volume is left alone.
pub fn set_muted(muted: bool) {
    let volume = if muted { 0.0 } else { 1.0 };
    STICKY.lock().unwrap().1 = Some(volume);
    shadow_call("setVolume", json!([volume]));
}

/// The output device's volume as last read or set.
pub fn device_volume() -> Option<f32> {
    *DEVICE_VOLUME.lock().unwrap()
}

/// Whether the output device plays `rate` as it is (None when its rates are unknown).
/// A rate it lacks (44.1 kHz on a 48/96 kHz-only device, say) is resampled by PipeWire.
pub fn device_plays(rate: u32) -> Option<bool> {
    let guard = DEVICE_FORMAT.lock().unwrap();
    let rates = guard.as_ref()?.1.as_ref()?;
    Some(rates.rates.contains(&rate))
}

/// Stop the stream (a non-Qobuz track plays next, or Qobuz playback ends) and give
/// the PipeWire rate back.
pub fn stop_audio() {
    *SUCCESSOR.lock().unwrap() = None;
    GEN.fetch_add(1, Ordering::SeqCst);
    *SHADOW.write().unwrap() = None;
    shadow_call("stop", json!([]));
    if FORCED_RATE.swap(0, Ordering::SeqCst) != 0 {
        std::thread::spawn(qbz_audio::pipewire_backend::PipeWireBackend::release_forced_rate);
    }
}

/// The stream's latest state for the current start (`None` until it has answered).
pub fn shadow() -> Option<ShadowState> {
    let gen = GEN.load(Ordering::SeqCst);
    SHADOW.read().ok().and_then(|s| s.clone()).filter(|s| s.gen == gen)
}

/// Sign-in wanted (`qobuz_engine::set_login_open`): when the player is signed out,
/// Qobuz's page opens in a Firefox window.
pub fn set_login_open(open: bool) {
    LOGIN_WANTED.store(open, Ordering::SeqCst);
}

// --- the host -----------------------------------------------------------------

enum Exit {
    SignIn,
    /// The output device changed: start Firefox again on the new one.
    Restart,
    Shutdown,
}

async fn host(mut rx: mpsc::UnboundedReceiver<Cmd>) {
    let mut backoff = Duration::from_secs(1);
    let mut first = true;
    loop {
        if SHUTDOWN.load(Ordering::SeqCst) {
            return;
        }
        if let Err(e) = firefox_binary().await {
            log::warn!("[firefox-engine] {e}");
            crate::toast_qt::error(qbz_i18n::t(
                "Couldn't set up Firefox for Qobuz playback. Check the connection, install Firefox, or start ZBQ with ZBQ_QOBUZ_AUDIO=web.",
            ));
            tokio::time::sleep(Duration::from_secs(60)).await;
            continue;
        }
        let result = headless(&mut rx, first).await;
        first = false;
        match result {
            Ok(Exit::Shutdown) => return,
            Ok(Exit::Restart) => backoff = Duration::from_secs(1),
            Ok(Exit::SignIn) => {
                backoff = Duration::from_secs(1);
                sign_in_window().await;
            }
            Err(e) => {
                log::warn!("[firefox-engine] {e}; starting Firefox again in {} s", backoff.as_secs());
                if crate::qobuz_playback::engine_owns_playback() {
                    crate::qobuz_playback::release();
                    crate::toast_qt::error(qbz_i18n::t("Qobuz's player stopped unexpectedly. Press play to continue."));
                }
                tokio::time::sleep(backoff).await;
                backoff = (backoff * 2).min(Duration::from_secs(60));
            }
        }
    }
}

const STATE_JS: &str = "(function () { var e = window.__zbq ? window.__zbq.state() : ''; \
     var s = window.__zbqShadow ? window.__zbqShadow.state() : ''; return JSON.stringify({ e: e, s: s }); })()";

fn page_script() -> String {
    format!(
        "() => {{\n{}\n{}\ndocument.addEventListener('DOMContentLoaded', function () {{\n{}\n}}, {{ once: true }});\n}}",
        include_str!("../engine/qobuz_engine_shadow.js"),
        include_str!("../engine/qobuz_engine_early.js"),
        include_str!("../engine/qobuz_engine_bridge.js"),
    )
}

/// The PipeWire sink for the output device chosen in ZBQ's audio settings (None =
/// the system default).
fn configured_sink() -> Option<String> {
    let device = crate::settings_qt::audio_settings()
        .output_device
        .filter(|d| !d.trim().is_empty() && d != "default")?;
    out::Graph::read().resolve_sink(&device)
}

/// The sink Firefox plays to: the one it was started on, or the system default.
async fn current_sink() -> Option<String> {
    if let Some(sink) = LAUNCH_SINK.lock().unwrap().clone() {
        return Some(sink);
    }
    tokio::task::spawn_blocking(out::default_sink).await.ok().flatten()
}

async fn headless(rx: &mut mpsc::UnboundedReceiver<Cmd>, first: bool) -> Result<Exit, String> {
    let binary = firefox_binary().await?;
    let profile = profile_dir()?;
    clear_stale_instance(&profile).await;
    let sink = tokio::task::spawn_blocking(configured_sink).await.ok().flatten();
    *LAUNCH_SINK.lock().unwrap() = sink.clone();
    let port = free_port()?;
    let mut child = spawn_firefox(&binary, &profile, Some(port), sink.as_deref())?;
    let result = drive(&mut child, port, rx, first, sink).await;
    terminate(&mut child).await;
    result
}

async fn drive(
    child: &mut Child,
    port: u16,
    rx: &mut mpsc::UnboundedReceiver<Cmd>,
    first: bool,
    launch_sink: Option<String>,
) -> Result<Exit, String> {
    let bidi = connect_when_up(port, child).await?;
    bidi.call("session.new", json!({ "capabilities": {} }), Duration::from_secs(30)).await?;
    let tree = bidi.call("browsingContext.getTree", json!({}), Duration::from_secs(10)).await?;
    let ctx = tree
        .pointer("/contexts/0/context")
        .and_then(Value::as_str)
        .ok_or("Firefox has no page")?
        .to_string();
    bidi.call("script.addPreloadScript", json!({ "functionDeclaration": page_script() }), Duration::from_secs(10))
        .await?;
    navigate(&bidi, &ctx).await?;
    log::info!(
        "[firefox-engine] Qobuz's player is loading in Firefox{} (output: {})",
        if first { "" } else { " again" },
        launch_sink.as_deref().unwrap_or("system default")
    );
    // Whatever was asked while Firefox was down is stale; the quality ceiling and the
    // stream volume are re-sent once the player is ready.
    while rx.try_recv().is_ok() {}
    let mut sticky_sent = false;
    let mut last_poll = Instant::now() - Duration::from_secs(1);
    let mut last_rate_check = Instant::now();
    let mut rate_reasserted = false;
    let mut last_volume_read = Instant::now() - Duration::from_secs(10);
    let mut last_device_check = Instant::now();
    let mut tick = tokio::time::interval(Duration::from_millis(50));
    loop {
        tick.tick().await;
        if SHUTDOWN.load(Ordering::SeqCst) {
            return Ok(Exit::Shutdown);
        }
        if let Ok(Some(status)) = child.try_wait() {
            return Err(format!("Firefox exited ({status})"));
        }
        if bidi.is_closed() {
            return Err("lost the connection to Firefox".into());
        }
        while let Ok(cmd) = rx.try_recv() {
            run_command(&bidi, &ctx, cmd).await;
        }
        if last_poll.elapsed() < Duration::from_millis(50) {
            continue;
        }
        last_poll = Instant::now();
        let raw = match bidi.evaluate(&ctx, STATE_JS).await {
            Ok(Value::String(s)) => s,
            Ok(_) => continue,
            Err(e) => {
                log::debug!("[firefox-engine] state poll failed: {e}");
                continue;
            }
        };
        let Ok(outer) = serde_json::from_str::<Value>(&raw) else { continue };
        if let Some(engine) = outer.get("e").and_then(Value::as_str).filter(|s| !s.is_empty()) {
            crate::qobuz_engine::on_state_json(engine);
        }
        let engine_state = crate::qobuz_engine::state().unwrap_or_default();
        if engine_state.ready && !sticky_sent {
            sticky_sent = true;
            let (format, volume) = *STICKY.lock().unwrap();
            if let Some(format) = format {
                run_command(&bidi, &ctx, Cmd::MaxFormat(format)).await;
            }
            if let Some(volume) = volume {
                let _ = bidi.evaluate(&ctx, &format!("window.__zbqShadow && window.__zbqShadow.setVolume({volume})")).await;
            }
        }
        if let Some(shadow) = outer.get("s").and_then(Value::as_str).filter(|s| !s.is_empty()) {
            if let Ok(mut state) = serde_json::from_str::<ShadowState>(shadow) {
                for line in state.log.drain(..) {
                    log::info!("[qobuz-stream] {line}");
                }
                if state.need_rate > 0 {
                    set_output_rate(state.need_rate).await;
                    rate_reasserted = false;
                    let _ = bidi.evaluate(&ctx, &format!("window.__zbqShadow.ack({})", state.need_rate)).await;
                } else if state.started && state.rate > 0 && !rate_reasserted && last_rate_check.elapsed() > Duration::from_secs(3) {
                    last_rate_check = Instant::now();
                    if FORCED_RATE.load(Ordering::SeqCst) == state.rate {
                        let now = tokio::task::spawn_blocking(qbz_audio::pipewire_backend::PipeWireBackend::forced_rate)
                            .await
                            .unwrap_or(0);
                        // Once per run: another app that sets its own rate is not fought
                        // over (rate flips every few seconds are hard on some devices).
                        if now != state.rate {
                            log::warn!("[firefox-engine] PipeWire left {} Hz (now {now}); setting it again", state.rate);
                            rate_reasserted = true;
                            set_output_rate(state.rate).await;
                        }
                    }
                }
                state.received_at = Some(Instant::now());
                *SHADOW.write().unwrap() = Some(state);
            }
        }
        // The device's volume, for the app's volume control.
        if last_volume_read.elapsed() > Duration::from_secs(1) {
            last_volume_read = Instant::now();
            let sink = current_sink().await;
            let volume = tokio::task::spawn_blocking(move || {
                let graph = out::Graph::read();
                let id = graph.linked_sink(CHILD_PID.load(Ordering::SeqCst)).map(|(id, _)| id).or_else(|| graph.sink_id(sink.as_deref()?))?;
                out::volume(id)
            })
            .await
            .ok()
            .flatten();
            if volume.is_some() {
                *DEVICE_VOLUME.lock().unwrap() = volume;
            }
        }
        // A different output device chosen in the settings: start again on it, once
        // nothing is playing.
        if last_device_check.elapsed() > Duration::from_secs(3) {
            last_device_check = Instant::now();
            let now = tokio::task::spawn_blocking(configured_sink).await.ok().flatten();
            if now != launch_sink && !crate::qobuz_playback::engine_owns_playback() {
                log::info!("[firefox-engine] output device changed to {}", now.as_deref().unwrap_or("the system default"));
                return Ok(Exit::Restart);
            }
        }
        if LOGIN_WANTED.load(Ordering::SeqCst) && engine_state.ready && !engine_state.logged_in {
            log::info!("[firefox-engine] sign-in wanted: opening Qobuz's page in a Firefox window");
            return Ok(Exit::SignIn);
        }
    }
}

async fn run_command(bidi: &Bidi, ctx: &str, cmd: Cmd) {
    match cmd {
        Cmd::Js(js) => {
            if let Err(e) = bidi.evaluate(ctx, &js).await {
                log::debug!("[firefox-engine] page call failed: {e}");
            }
        }
        Cmd::MaxFormat(requested) => {
            let format = capped_format(requested).await;
            let _ = bidi
                .evaluate(ctx, &format!("window.__zbq && window.__zbq.setMaxFormat && window.__zbq.setMaxFormat({format})"))
                .await;
        }
        Cmd::SeekInStream(ms) => {
            match bidi.evaluate(ctx, &format!("window.__zbqShadow ? window.__zbqShadow.seekTo({ms}) : false")).await {
                Ok(Value::Bool(true)) => {}
                other => log::warn!("[firefox-engine] seek to {ms} ms inside the stream refused: {other:?}"),
            }
        }
        Cmd::Retarget(ms) => {
            let _ = bidi.evaluate(ctx, &format!("window.__zbqShadow && window.__zbqShadow.retarget({ms})")).await;
        }
        Cmd::Pause(pause) => {
            let js = if pause { "window.__zbqShadow && window.__zbqShadow.pause(true)" } else { "window.__zbqShadow && window.__zbqShadow.resume()" };
            let _ = bidi.evaluate(ctx, js).await;
        }
        Cmd::DeviceVolume(volume) => {
            let sink = current_sink().await;
            let set = tokio::task::spawn_blocking(move || {
                let graph = out::Graph::read();
                let id = graph.linked_sink(CHILD_PID.load(Ordering::SeqCst)).map(|(id, _)| id).or_else(|| graph.sink_id(sink.as_deref()?))?;
                out::set_volume(id, volume).then_some(())
            })
            .await
            .ok()
            .flatten();
            if set.is_some() {
                *DEVICE_VOLUME.lock().unwrap() = Some(volume);
            }
        }
    }
}

/// The quality ceiling for Qobuz: what the app asks for, never above what the output
/// device plays without resampling.
async fn capped_format(requested: u32) -> u32 {
    let Some(sink) = current_sink().await else { return requested };
    match device_caps(&sink).await {
        Some(rates) if out::format_rank(rates.format) < out::format_rank(requested) => {
            log::info!(
                "[firefox-engine] {sink} plays up to {} Hz: Qobuz is asked for format {} at most",
                rates.max_hz,
                rates.format
            );
            rates.format
        }
        _ => requested,
    }
}

/// What `sink` plays without resampling, read from the device once (a failed read
/// is tried again next time).
async fn device_caps(sink: &str) -> Option<out::DeviceRates> {
    if let Some((cached, caps)) = DEVICE_FORMAT.lock().unwrap().clone() {
        if cached == sink {
            return caps;
        }
    }
    let probe = sink.to_string();
    let caps = tokio::task::spawn_blocking(move || out::device_rates(&probe)).await.ok().flatten()?;
    log::info!("[firefox-engine] {sink} plays {:?} Hz", caps.rates);
    *DEVICE_FORMAT.lock().unwrap() = Some((sink.to_string(), Some(caps.clone())));
    Some(caps)
}

/// Set the PipeWire graph to `rate` before a run of tracks at that rate starts, as
/// the native PipeWire backend does for its own streams ("Lock output device" in the
/// audio settings opts out, as it does there). A rate the device lacks is never set:
/// the nearest one it has is, and PipeWire resamples the track to it. Read from
/// PipeWire each time: anything else may have changed it since. Returns once the
/// device runs at it (or is idle, and will start at it).
async fn set_output_rate(rate: u32) {
    use qbz_audio::pipewire_backend::PipeWireBackend;
    if crate::settings_qt::audio_settings().skip_sink_switch {
        log::info!("[firefox-engine] output device locked: PipeWire may resample {rate} Hz");
        return;
    }
    let sink = current_sink().await;
    let caps = match &sink {
        Some(sink) => device_caps(sink).await,
        None => None,
    };
    let rate = match caps {
        Some(caps) if !caps.rates.contains(&rate) => {
            let nearest = PipeWireBackend::find_best_fallback_rate(rate, &caps.rates);
            log::warn!("[firefox-engine] the output device has no {rate} Hz (it plays {:?}): PipeWire resamples to {nearest} Hz", caps.rates);
            nearest
        }
        _ => rate,
    };
    if tokio::task::spawn_blocking(PipeWireBackend::forced_rate).await.unwrap_or(0) == rate {
        FORCED_RATE.store(rate, Ordering::SeqCst);
        return;
    }
    let ok = tokio::task::spawn_blocking(move || PipeWireBackend::force_pipewire_rate(rate)).await.unwrap_or(false);
    if !ok {
        return;
    }
    FORCED_RATE.store(rate, Ordering::SeqCst);
    for _ in 0..40 {
        let probe = sink.clone();
        let state = tokio::task::spawn_blocking(move || out::Graph::read().sink_state(probe.as_deref()?)).await.ok().flatten();
        match state {
            Some((state, now)) if state == "running" && now != rate => tokio::time::sleep(Duration::from_millis(25)).await,
            _ => break,
        }
    }
}

async fn navigate(bidi: &Bidi, ctx: &str) -> Result<(), String> {
    let mut attempt = 0u32;
    loop {
        match bidi
            .call(
                "browsingContext.navigate",
                json!({ "context": ctx, "url": PLAYER_URL, "wait": "complete" }),
                Duration::from_secs(60),
            )
            .await
        {
            Ok(_) => return Ok(()),
            Err(e) if attempt < 6 => {
                let wait = Duration::from_secs(2u64.pow(attempt.min(5)));
                log::warn!("[firefox-engine] loading Qobuz's player failed ({e}); retrying in {} s", wait.as_secs());
                tokio::time::sleep(wait).await;
                attempt += 1;
            }
            Err(e) => return Err(e),
        }
    }
}

fn free_port() -> Result<u16, String> {
    let listener = std::net::TcpListener::bind(("127.0.0.1", 0)).map_err(|e| e.to_string())?;
    listener.local_addr().map(|a| a.port()).map_err(|e| e.to_string())
}

fn spawn_firefox(binary: &Path, profile: &Path, port: Option<u16>, sink: Option<&str>) -> Result<Child, String> {
    let mut cmd = Command::new(binary);
    cmd.arg("--no-remote").arg("--profile").arg(profile);
    match port {
        Some(port) => {
            cmd.arg("--headless").arg("--remote-debugging-port").arg(port.to_string()).arg("about:blank");
            cmd.env("MOZ_HEADLESS", "1");
        }
        None => {
            cmd.arg(PLAYER_URL);
            cmd.env_remove("MOZ_HEADLESS");
        }
    }
    // The sound system shows (and remembers settings for) ZBQ, not "Firefox".
    cmd.env("PULSE_PROP_OVERRIDE", "application.name='ZBQ' application.icon_name='lol.zbq.ZBQ' media.role='music'");
    // The launcher's Qt library path is for ZBQ, not for Firefox.
    cmd.env_remove("LD_LIBRARY_PATH");
    // The output device chosen in ZBQ's audio settings (else the system default).
    if let Some(sink) = sink {
        cmd.env("PULSE_SINK", sink);
    }
    cmd.stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null()).kill_on_drop(true);
    #[cfg(target_os = "linux")]
    unsafe {
        // Firefox goes when the app goes, even if the app is killed.
        cmd.pre_exec(|| {
            libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGTERM);
            Ok(())
        });
    }
    let child = cmd.spawn().map_err(|e| format!("starting Firefox: {e}"))?;
    CHILD_PID.store(child.id().unwrap_or(0), Ordering::SeqCst);
    Ok(child)
}

async fn connect_when_up(port: u16, child: &mut Child) -> Result<std::sync::Arc<Bidi>, String> {
    let started = Instant::now();
    loop {
        if let Ok(Some(status)) = child.try_wait() {
            return Err(format!("Firefox exited at start ({status})"));
        }
        if tokio::net::TcpStream::connect(("127.0.0.1", port)).await.is_ok() {
            return Bidi::connect(port).await;
        }
        if started.elapsed() > Duration::from_secs(45) {
            return Err("Firefox did not open its remote port".into());
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
}

async fn terminate(child: &mut Child) {
    if let Some(pid) = child.id() {
        unsafe {
            libc::kill(pid as i32, libc::SIGTERM);
        }
        if tokio::time::timeout(Duration::from_secs(5), child.wait()).await.is_err() {
            let _ = child.kill().await;
        }
    }
    CHILD_PID.store(0, Ordering::SeqCst);
}

/// A Firefox left running on this profile (the app crashed) would hold its lock.
async fn clear_stale_instance(profile: &Path) {
    let Ok(target) = std::fs::read_link(profile.join("lock")) else { return };
    let Some(pid) = target.to_string_lossy().rsplit('+').next().and_then(|p| p.parse::<i32>().ok()) else { return };
    let cmdline = std::fs::read(format!("/proc/{pid}/cmdline")).unwrap_or_default();
    if !String::from_utf8_lossy(&cmdline).contains(&*profile.to_string_lossy()) {
        return;
    }
    log::info!("[firefox-engine] stopping a Firefox left running on ZBQ's profile");
    unsafe {
        libc::kill(pid, libc::SIGTERM);
    }
    for _ in 0..50 {
        if !Path::new(&format!("/proc/{pid}")).exists() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    unsafe {
        libc::kill(pid, libc::SIGKILL);
    }
}

/// The page's stored session (`localuser`), read from the profile's local storage.
/// Firefox holds that database locked while it runs, so a copy is read (inside the
/// profile, which only the user can open) and removed again.
fn stored_session(profile: &Path) -> Option<Vec<u8>> {
    let db = profile.join("storage/default/https+++play.qobuz.com/ls/data.sqlite");
    if !db.is_file() {
        return None;
    }
    let copy = profile.join(".zbq-session-check.sqlite");
    std::fs::copy(&db, &copy).ok()?;
    let value = rusqlite::Connection::open_with_flags(
        &copy,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .ok()
    .and_then(|conn| {
        conn.query_row("SELECT value FROM data WHERE key = 'localuser'", [], |row| row.get::<_, Vec<u8>>(0))
            .ok()
    });
    let _ = std::fs::remove_file(&copy);
    value.filter(|v| v.len() > 100)
}

/// Qobuz's own sign-in page in a normal Firefox window; returns when the session is
/// in the profile (the window is then closed), the window was closed, or the
/// sign-in was cancelled.
async fn sign_in_window() {
    let Ok(profile) = profile_dir() else { return };
    let Ok(binary) = firefox_binary().await else { return };
    clear_stale_instance(&profile).await;
    let before = {
        let p = profile.clone();
        tokio::task::spawn_blocking(move || stored_session(&p)).await.ok().flatten()
    };
    let mut child = match spawn_firefox(&binary, &profile, None, None) {
        Ok(child) => child,
        Err(e) => {
            log::warn!("[firefox-engine] sign-in window: {e}");
            return;
        }
    };
    let opened = Instant::now();
    loop {
        tokio::time::sleep(Duration::from_secs(1)).await;
        if SHUTDOWN.load(Ordering::SeqCst) || !LOGIN_WANTED.load(Ordering::SeqCst) {
            log::info!("[firefox-engine] sign-in window closed by the app");
            break;
        }
        if let Ok(Some(_)) = child.try_wait() {
            CHILD_PID.store(0, Ordering::SeqCst);
            let p = profile.clone();
            let now = tokio::task::spawn_blocking(move || stored_session(&p)).await.ok().flatten();
            if now.is_none() || now == before {
                // Closed without signing in: the same as the in-app Cancel.
                log::info!("[firefox-engine] sign-in window closed without a sign-in");
                LOGIN_WANTED.store(false, Ordering::SeqCst);
                crate::qobuz_playback::cancel_pending();
                crate::cancel_login();
            } else {
                log::info!("[firefox-engine] sign-in window closed after signing in");
            }
            return;
        }
        let now = {
            let p = profile.clone();
            tokio::task::spawn_blocking(move || stored_session(&p)).await.ok().flatten()
        };
        if now.is_some() && now != before {
            log::info!("[firefox-engine] signed in on Qobuz's page; closing the window");
            // Let the page finish writing before Firefox goes.
            tokio::time::sleep(Duration::from_secs(2)).await;
            break;
        }
        if opened.elapsed() > SIGN_IN_LIMIT {
            break;
        }
    }
    terminate(&mut child).await;
}

#[cfg(test)]
mod tests {
    use super::is_snap_firefox;

    #[test]
    fn snap_wrappers_are_not_a_firefox_to_drive() {
        let dir = std::env::temp_dir().join(format!("zbq-snap-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let wrapper = dir.join("ubuntu-firefox");
        std::fs::write(&wrapper, "#!/bin/sh\n# transitional\nexec /snap/bin/firefox \"$@\"\n").unwrap();
        let script = dir.join("plain-script");
        std::fs::write(&script, "#!/bin/sh\nexec /opt/firefox/firefox \"$@\"\n").unwrap();
        let binary = dir.join("firefox");
        std::fs::write(&binary, b"\x7fELF\x02\x01\x01 not a script /snap/ inside a binary").unwrap();
        assert!(is_snap_firefox(&wrapper));
        assert!(!is_snap_firefox(&script));
        assert!(!is_snap_firefox(&binary));
        assert!(is_snap_firefox(std::path::Path::new("/snap/bin/firefox")));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
