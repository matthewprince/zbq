//! QbzQobuzEngine: the QML side of the official-player engine (`qobuz_engine.rs`).
//!
//! `qml/engine/QobuzEngine.qml` hosts Qobuz's official web player in a WebEngineView,
//! injects the two scripts published here, polls `window.__zbq.state()` and hands
//! every answer to [`QbzQobuzEngine::on_state`]. Rust asks the page to act through the
//! `command` signal (the QML runs `window.__zbq[name](...args)`). `loginOpen` shows
//! the page full-size so the user can sign in on Qobuz's own login page.
//!
//! With Firefox installed the official player runs there instead (`firefox_engine.rs`):
//! this view then stays on a blank page and only the sign-in banner shows.
//!
//! Glue only; behaviour lives in `qobuz_engine.rs` and `auth_qt.rs`.

use std::pin::Pin;
use std::sync::OnceLock;

use cxx_qt::CxxQtThread;
use cxx_qt::Threading as _;
use cxx_qt_lib::QString;

#[cxx_qt::bridge]
pub mod qbz_qobuz_engine_bridge {
    extern "C++" {
        include!("cxx-qt-lib/qstring.h");
        type QString = cxx_qt_lib::QString;
    }

    #[auto_cxx_name]
    extern "RustQt" {
        #[qobject]
        #[qml_element]
        #[qml_singleton]
        /// Injected at DocumentCreation, before Qobuz's bundle runs.
        #[qproperty(QString, early_script)]
        /// Injected at DocumentReady: the control surface (`window.__zbq`).
        #[qproperty(QString, bridge_script)]
        /// The official player's address.
        #[qproperty(QString, engine_url)]
        /// Show the page full-size for sign-in.
        #[qproperty(bool, login_open)]
        /// The official player runs in Firefox (`firefox_engine.rs`), not here.
        #[qproperty(bool, firefox_mode)]
        /// No Firefox found: ask whether to download Firefox ESR (`Main.qml`).
        #[qproperty(bool, firefox_prompt)]
        type QbzQobuzEngine = super::QbzQobuzEngineRust;

        /// Registers the Qt-thread hop. Called from `Main.qml`'s boot sweep and,
        /// defensively, from every invokable.
        #[qinvokable]
        fn boot(self: Pin<&mut QbzQobuzEngine>);

        /// The page's latest `window.__zbq.state()` answer (a JSON string).
        #[qinvokable]
        fn on_state(self: Pin<&mut QbzQobuzEngine>, json: QString);

        /// The login overlay's close button: give up on this sign-in attempt.
        #[qinvokable]
        fn cancel_login(self: Pin<&mut QbzQobuzEngine>);

        /// The Firefox prompt's answer: download Firefox ESR now, or not now.
        #[qinvokable]
        fn answer_firefox_prompt(self: Pin<&mut QbzQobuzEngine>, download: bool);

        /// Ask the page to run `window.__zbq[name](...JSON.parse(args))`.
        #[qsignal]
        fn command(self: Pin<&mut QbzQobuzEngine>, name: QString, args: QString);
    }

    impl cxx_qt::Threading for QbzQobuzEngine {}
}

use qbz_qobuz_engine_bridge::QbzQobuzEngine;

/// Rust side of the engine bridge (plain storage).
pub struct QbzQobuzEngineRust {
    early_script: QString,
    bridge_script: QString,
    engine_url: QString,
    login_open: bool,
    firefox_mode: bool,
    firefox_prompt: bool,
}

impl Default for QbzQobuzEngineRust {
    fn default() -> Self {
        let firefox_mode = crate::firefox_engine::enabled();
        Self {
            early_script: QString::from(include_str!("../engine/qobuz_engine_early.js")),
            bridge_script: QString::from(include_str!("../engine/qobuz_engine_bridge.js")),
            // ZBQ_ENGINE_URL: a development override; unset in normal use.
            engine_url: QString::from(if firefox_mode {
                "about:blank".to_string()
            } else {
                std::env::var("ZBQ_ENGINE_URL").unwrap_or_else(|_| "https://play.qobuz.com/".to_string())
            }
            .as_str()),
            login_open: false,
            firefox_mode,
            firefox_prompt: false,
        }
    }
}

static QT_THREAD: OnceLock<CxxQtThread<QbzQobuzEngine>> = OnceLock::new();

/// Queue an engine-bridge call onto the Qt event loop (no-op before the hop exists).
pub(crate) fn ui(f: impl FnOnce(Pin<&mut QbzQobuzEngine>) + Send + 'static) {
    if let Some(thread) = QT_THREAD.get() {
        let _ = thread.queue(f);
    }
}

impl qbz_qobuz_engine_bridge::QbzQobuzEngine {
    fn register_thread(&self) {
        if QT_THREAD.get().is_none() {
            let _ = QT_THREAD.set(self.qt_thread());
        }
    }

    pub fn boot(self: Pin<&mut Self>) {
        self.register_thread();
        crate::firefox_engine::start();
        // ZBQ_DEV_LOGIN=1: press "Sign in with Qobuz" once the official player is
        // ready, signed in or not (a check of the sign-in flow).
        if std::env::var("ZBQ_DEV_LOGIN").is_ok() {
            crate::spawn(async {
                for _ in 0..240 {
                    tokio::time::sleep(std::time::Duration::from_millis(250)).await;
                    if crate::qobuz_engine::state().is_some_and(|s| s.ready) {
                        log::info!("[dev] ZBQ_DEV_LOGIN: starting the sign-in");
                        crate::start_login();
                        return;
                    }
                }
            });
        }
        // ZBQ_DEV_QOBUZ_PLAY=<track id>: development check of the real flow:
        // once Qobuz's player is ready and signed in, play that track through
        // `qobuz_playback::start`, exactly as the queue would.
        // ZBQ_DEV_QOBUZ_ALBUM=<album id>:<index>: the same, but through the
        // app's album play (a real queue that advances by itself).
        let album = std::env::var("ZBQ_DEV_QOBUZ_ALBUM").ok().and_then(|v| {
            let (album, index) = v.split_once(':')?;
            Some((album.to_string(), index.parse::<usize>().ok()?))
        });
        let single = std::env::var("ZBQ_DEV_QOBUZ_PLAY").ok().and_then(|v| v.parse::<u64>().ok());
        // ZBQ_DEV_QOBUZ_TRACKS=<id>,<id>,...: the same with exactly these tracks as
        // the queue (e.g. a sample-rate change between two tracks).
        let tracks = std::env::var("ZBQ_DEV_QOBUZ_TRACKS")
            .ok()
            .map(|v| v.split(',').filter_map(|t| t.trim().parse::<u64>().ok()).collect::<Vec<_>>())
            .filter(|ids| !ids.is_empty());
        if single.is_some() || album.is_some() || tracks.is_some() {
            crate::spawn(async move {
                for _ in 0..360 {
                    tokio::time::sleep(std::time::Duration::from_millis(250)).await;
                    let ready = crate::qobuz_engine::state().is_some_and(|s| s.ready && s.logged_in);
                    if ready {
                        let runtime = crate::app();
                        // ZBQ_DEV_SIGNIN=1: sign the app in first, exactly as the
                        // "Sign in with Qobuz" button does (the token comes from the
                        // signed-in official page), so the whole shell is running.
                        if std::env::var("ZBQ_DEV_SIGNIN").is_ok() {
                            crate::start_login();
                            let mut signed_in = false;
                            for _ in 0..120 {
                                tokio::time::sleep(std::time::Duration::from_millis(250)).await;
                                if runtime.core().has_session().await {
                                    signed_in = true;
                                    break;
                                }
                            }
                            if !signed_in {
                                log::warn!("[dev] ZBQ_DEV_SIGNIN: the app did not sign in");
                                return;
                            }
                            log::info!("[dev] app signed in through the official player");
                            tokio::time::sleep(std::time::Duration::from_secs(2)).await;
                        }
                        // The app's own session (restored from its saved token) can
                        // trail the page by a moment, and catalog calls need it.
                        for _ in 0..120 {
                            if runtime.core().has_session().await {
                                break;
                            }
                            tokio::time::sleep(std::time::Duration::from_millis(250)).await;
                        }
                        let _ = runtime.core().player().set_volume(1.0); // unity gain for the check
                        let result = match (&album, single, &tracks) {
                            (Some((album, index)), _, _) => crate::playback_qt::play_album_from(&runtime, album, *index).await,
                            (None, _, Some(ids)) => {
                                let mut list = Vec::new();
                                for id in ids {
                                    match crate::playback_qt::queue_track_for(&runtime, *id).await {
                                        Ok(track) => list.push(track),
                                        Err(e) => log::warn!("[dev] track {id}: {e}"),
                                    }
                                }
                                crate::playback_qt::play_track_list(&runtime, list, 0, false).await
                            }
                            (None, Some(id), None) => crate::qobuz_playback::start(&runtime, id, 0).await,
                            (None, None, None) => Ok(()),
                        };
                        if let Err(e) = result {
                            log::warn!("[dev] dev playback failed: {e}");
                            return;
                        }
                        // ZBQ_DEV_SCRIPT="10:pause;13:resume;20:seek=60": transport
                        // actions at seconds after the start, through the same calls the
                        // UI makes.
                        if let Ok(script) = std::env::var("ZBQ_DEV_SCRIPT") {
                            let started = std::time::Instant::now();
                            let mut steps: Vec<(f64, String)> = script
                                .split(';')
                                .filter_map(|step| {
                                    let (t, action) = step.split_once(':')?;
                                    Some((t.trim().parse().ok()?, action.trim().to_string()))
                                })
                                .collect();
                            steps.sort_by(|a, b| a.0.total_cmp(&b.0));
                            for (at, action) in steps {
                                let wait = at - started.elapsed().as_secs_f64();
                                if wait > 0.0 {
                                    tokio::time::sleep(std::time::Duration::from_secs_f64(wait)).await;
                                }
                                log::info!("[dev] {at}s: {action}");
                                match action.as_str() {
                                    "pause" => crate::qobuz_playback::set_playing(false),
                                    "resume" => crate::qobuz_playback::set_playing(true),
                                    // the now-playing bar's lyrics button
                                    "lyrics" => crate::toggle_lyrics(),
                                    // the volume bar and the mute button, through the UI's own calls
                                    "mute" => crate::playback_qt::toggle_mute(&runtime).await,
                                    other if other.starts_with("volume=") => {
                                        if let Ok(v) = other["volume=".len()..].parse::<f32>() {
                                            crate::playback_qt::set_volume(&runtime, v).await;
                                        }
                                    }
                                    other => {
                                        if let Some(target) = other.strip_prefix("seek=").and_then(|v| v.parse::<f64>().ok()) {
                                            let duration = crate::qobuz_playback::duration_secs().unwrap_or(0.0);
                                            if duration > 0.0 {
                                                crate::qobuz_playback::seek_frac((target / duration) as f32);
                                            }
                                        }
                                    }
                                }
                            }
                        }
                        return;
                    }
                }
                log::warn!("[dev] ZBQ_DEV_QOBUZ_PLAY: the official player never became ready and signed in");
            });
        }
    }

    pub fn on_state(self: Pin<&mut Self>, json: QString) {
        self.register_thread();
        // With the Firefox host the state comes from Firefox; this view is blank.
        if crate::firefox_engine::enabled() {
            return;
        }
        crate::qobuz_engine::on_state_json(&json.to_string());
    }

    pub fn cancel_login(self: Pin<&mut Self>) {
        self.register_thread();
        crate::qobuz_playback::cancel_pending();
        crate::cancel_login();
    }

    pub fn answer_firefox_prompt(self: Pin<&mut Self>, download: bool) {
        self.register_thread();
        crate::firefox_engine::answer_prompt(download);
    }
}
