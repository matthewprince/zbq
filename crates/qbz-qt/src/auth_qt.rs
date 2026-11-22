//! System-browser OAuth login for the Qt POC — a port of
//! `crates/qbz/src/auth.rs` onto cxx-qt, PLUS the redirect-listener
//! hardening from `crates/qbzd/src/login.rs` (CSPRNG nonce bound into the
//! redirect PATH; a mismatched/absent path nonce is dropped) that the
//! desktop auth.rs lacks.
//!
//! The three activation paths share ONE helper, `bind_per_user_stores`, plus
//! the phase-ordered calls above it; adding an account-scoped store means one
//! binding there and its matching teardown in `logout`.
//!
//! KEPT (the offline guardrails this phase is about):
//! `ensure_api_initialized` (scoped gate lift), `login_with_oauth_code`,
//! `core.set_session`, `runtime.activate`, offline-mode `init_for_user` +
//! `SubscriptionStateStore` bind + mark_valid/invalid (D4),
//! `engine().set_offline_session(false)` on success,
//! `qbz_credentials::save_oauth_token`, `qbz_log::register_secret`, and the
//! restore semantics (explicit auth rejection clears the token,
//! network-class failure keeps it).

use std::sync::Arc;
use std::time::Duration;

use qbz_app::shell::AppRuntime;
use qbz_app::user_data::UserDataPaths;
use qbz_core::FrontendAdapter;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

/// How long the official sign-in page may stay open waiting for the user.
const LOGIN_TIMEOUT: Duration = Duration::from_secs(600);

/// The authenticated user, as the shell needs it.
pub struct SessionInfo {
    pub display_name: String,
    pub subscription: String,
}

/// The plan label the header shows. Qobuz's `short_label` ("Studio",
/// "Sublime") is a proper noun and stays as sent; the one label QBZ makes up
/// itself — "Member", an account without a subscription — is translated.
pub fn display_subscription(label: &str) -> String {
    if label == "Member" {
        qbz_i18n::t("Member")
    } else {
        label.to_string()
    }
}

/// Progress milestones of the browser OAuth, reported to the login UI so
/// the screen can narrate the flow.
#[derive(Clone, Copy, Debug)]
pub enum LoginPhase {
    /// The browser was opened; waiting for the redirect on the listener.
    WaitingForBrowser,
    /// The authorization code was captured; exchanging it for a session.
    Authenticating,
}

/// Sign in on Qobuz's own login page, shown in the embedded official player, then
/// enter the session with the token that page holds. Returns the authenticated
/// session info on success. `on_phase` fires at each milestone (`WaitingForBrowser`
/// = the official page is showing, `Authenticating` = token received); it is called
/// from this async context, so it must hop to the UI thread itself.
///
/// Upstream finished Qobuz's external-app sign-in by exchanging the code with the
/// web player's private key, scraped from Qobuz's JavaScript bundle. That was
/// removed (see COMPLIANCE.md): the official page does its own sign-in and we only
/// read the resulting user token, the user's own credential.
pub async fn login_via_official_player<A, F>(
    runtime: &Arc<AppRuntime<A>>,
    on_phase: F,
) -> Result<SessionInfo, String>
where
    A: FrontendAdapter + Send + Sync + 'static,
    F: Fn(LoginPhase) + Send + Sync,
{
    let core = runtime.core();
    ensure_api_initialized(core).await?;

    let mut tokens = crate::qobuz_engine::token_watch();
    crate::qobuz_engine::set_login_open(true);
    on_phase(LoginPhase::WaitingForBrowser);
    let token = loop {
        if let Some(token) = tokens.borrow_and_update().clone() {
            break token;
        }
        match tokio::time::timeout(LOGIN_TIMEOUT, tokens.changed()).await {
            Ok(Ok(())) => continue,
            Ok(Err(_)) => {
                crate::qobuz_engine::set_login_open(false);
                return Err("Sign-in was interrupted".to_string());
            }
            Err(_) => {
                crate::qobuz_engine::set_login_open(false);
                return Err("Sign-in timed out".to_string());
            }
        }
    };
    crate::qobuz_engine::set_login_open(false);
    on_phase(LoginPhase::Authenticating);

    let session = {
        let client_lock = core.client();
        let guard = client_lock.read().await;
        let client = guard.as_ref().ok_or("Qobuz client not initialized")?;
        client.login_with_token(&token).await.map_err(|e| e.to_string())?
    };
    let user_id = session.user_id;
    let display_name = session.display_name.clone();
    let subscription = session.subscription_label.clone();
    let token = session.user_auth_token.clone();
    // Redaction (defense in depth): register the live auth token so any log
    // line that embeds it bare is scrubbed.
    qbz_log::register_secret(token.clone());

    // Emit LoggedIn through the core (idempotent set_session).
    crate::local_service_qt::quiesce().await;
    core.set_session(session).await.map_err(|e| e.to_string())?;

    // Activate the per-user session (creates dirs, opens the session store).
    runtime.activate(user_id).await?;

    // Offline-MODE per-user binding, then the D4 valid verdict and the D2
    // recovery: a successful login ends any unauthenticated offline session.
    if let Some(dir) = crate::offline_fwd::user_data_dir(user_id) {
        crate::local_service_qt::bind(&dir);
        crate::offline_fwd::init_for_user(&dir);
        // Plex settings live per-user (plex_settings.db); bind the store to
        // this session so the first Local Library read does not have to.
        crate::local_plex::init_for_user(&dir);
        // Session persistence (queue + current track + position), per user.
        qbz_app::session_persist::init_for_user(&dir);
        // Listen log (per-user listen_log.db).
        crate::listen_log_qt::init_for_user(&dir);
        // Local-favorites store (Library show-local + hearts).
        crate::library_qt::init_local_favorites(&dir);
        // Qobuz favourite-id cache (favorites_cache.db): correct hearts from
        // first paint.
        crate::fav_cache_qt::init_for_user(&dir);
        // Pinned-items store (AlbumCard pin badges).
        crate::sidebar_qt::init_pinned(&dir);
        // Intelligent-search service (cache + ranking).
        crate::search_qt::init(&dir, crate::search_qt::intelligent_search_pref());
        crate::search_cache_qt::init();
        // Playlist is_owner math.
        crate::playlist_qt::set_user_id(user_id);
        bind_per_user_stores(&dir, user_id).await;
    }
    crate::offline_fwd::engine().set_offline_session(false);
    crate::media_sync_qt::resume_jellyfin_quality();

    // Persist the token so the next launch restores the session silently.
    if let Err(e) = qbz_credentials::save_oauth_token(&token) {
        log::warn!("[qbz-qt] failed to persist the Qobuz token: {e}");
    }

    log::info!("[qbz-qt] login complete for user {user_id}");
    Ok(SessionInfo {
        display_name,
        subscription,
    })
}

/// Per-user stores bound on EVERY session activation — fresh login, silent
/// restore and the offline entry alike.
///
/// One helper rather than three copies, because the three activation paths
/// drifting apart is not a hypothetical: `qbz/src/artist_blacklist.rs:41-46`
/// records that Tauri bound the blacklist on login and restore but NOT
/// offline, so blacklisted artists leaked into every offline surface. Anything
/// added here lands on all three by construction.
///
/// `myqbz_qt::init_for_user` also runs the mixtape migrations (best-effort,
/// logs on failure, never blocks shell entry) — one entry point, so the caller
/// cannot bind the uid and forget the schema.
async fn bind_per_user_stores(dir: &std::path::Path, user_id: u64) {
    crate::local_service_qt::bind(dir);
    // Independent Last.fm/ListenBrainz credentials + queues. This binding is
    // retained across Qobuz logout by design (opt-out lives in that store),
    // and is replaced atomically when another profile activates.
    crate::integrations_qt::init_for_user(dir);
    // Invalidate any late media-server response before the registry moves to a
    // different profile. Epoch checks inside every page transaction then keep
    // the previous user's catalog out of this user's cache.
    crate::media_sync_qt::cancel_all();
    let jellyfin_gate = crate::media_sync_qt::media_server_state_guard(
        qbz_app::settings::media_servers::MediaServerKind::Jellyfin,
    );
    let subsonic_gate = crate::media_sync_qt::media_server_state_guard(
        qbz_app::settings::media_servers::MediaServerKind::Subsonic,
    );
    // THE SOURCE REGISTRY FIRST, before any other store. `myqbz_qt::
    // init_for_user` below runs the mixtape migrations through
    // `library_db_qt::with_db(true, …)`, and against an unbound pool a fresh
    // account can never create a collection. It is also what makes local and
    // Plex playback work at all: an unbound `DbPool` refuses every read, and
    // `PlexSource` with no credentials answers `NotConfigured` — which is
    // exactly what shipped for one build when this line was missing.
    // The media-server settings store FIRST: `source_wiring::bind_user` ends
    // with `report_wiring`, which asks it which remote sources are configured.
    // Bound after, that probe would report "none" on every login.
    crate::media_servers_qt::init_for_user(dir);
    crate::source_wiring::bind_user(dir, user_id);
    drop(subsonic_gate);
    drop(jellyfin_gate);
    // The filter chips are gated on "is this server configured", and the
    // answer is known now — without this the chips are absent until the first
    // browse reload, which for a user who never touches settings is never.
    crate::local_bridge_ops::publish_media_gates();
    // MyQBZ: branding (label + icon, read by the sidebar/nav flyout on frame
    // 1) and the grids + mixtape schema.
    crate::myqbz_prefs_qt::init_for_user(dir);
    crate::myqbz_qt::init_for_user(dir, user_id);
    // Artist/album blacklist. Until this binds, every read fail-opens
    // (`is_enabled()` → true, empty manager) and every mutation refuses with
    // "No active session" — the feature looks broken but never panics.
    crate::artist_blacklist::init_for_user(dir);
    // "Not interested" dismissals on Recommendations cards.
    crate::reco_dismiss_qt::init_for_user(dir);
    // Purchases ownership is account-scoped. Reset before warming so a late
    // response from the previous account cannot grant AlbumView an action.
    crate::purchases_qt::activate_profile();
    // The catalog sidecar is profile-scoped. Coalescing here covers first
    // login and account switches even when the one-shot boot worker began on
    // the previous/guest profile.
    crate::local_catalog_qt::request_catch_up();
}

/// Make sure the Qobuz client is set up before a sign-in call. Scoped gate
/// lift around the gated cold client init — port of the desktop pattern
/// (crates/qbz/src/auth.rs `ensure_api_initialized`).
async fn ensure_api_initialized<A>(core: &qbz_core::QbzCore<A>) -> Result<(), String>
where
    A: FrontendAdapter + Send + Sync + 'static,
{
    if core.is_api_initialized().await {
        return Ok(());
    }
    let offline_engine = crate::offline_fwd::engine();
    let lifted = offline_engine.status().offline_session;
    if lifted {
        log::info!(
            "[qbz-qt] cold bundle init with an offline session active — lifting the flag for the init only"
        );
        offline_engine.set_offline_session(false);
    }
    let result = core.try_init_api().await.map_err(|e| e.to_string());
    if lifted {
        offline_engine.set_offline_session(true);
    }
    result
}

/// True only for an EXPLICIT auth rejection from Qobuz: a 401 on the token
/// login or an ineligible-account verdict. Network-class failures return
/// false — on those the saved token must be KEPT (spec §4.1 D1).
fn is_auth_rejection(error: &qbz_core::CoreError) -> bool {
    matches!(
        error,
        qbz_core::CoreError::Api(
            qbz_qobuz::ApiError::AuthenticationError(_) | qbz_qobuz::ApiError::IneligibleUser
        )
    )
}

/// Shared validation boundary, before any store/session activation. The
/// callback is the only token-store write and only explicit auth rejection
/// reaches it. Network errors reach the existing login/offline UI as errors.
fn validated_saved_session(
    result: Result<qbz_models::UserSession, qbz_core::CoreError>,
    clear_token: impl FnOnce(),
) -> Result<Option<qbz_models::UserSession>, String> {
    match result {
        Ok(session) => Ok(Some(session)),
        Err(error) if is_auth_rejection(&error) => {
            log::warn!("[qbz-qt] saved token rejected by Qobuz, clearing: {error}");
            clear_token();
            Ok(None)
        }
        Err(error) => {
            log::warn!("[qbz-qt] session restore failed, keeping saved token: {error}");
            Err(error.to_string())
        }
    }
}

/// The boot controller's single completion point. A finished restore always
/// releases the splash; the login view already contains the offline action.
/// Neither this controller nor the request timeout cancels store activation.
pub(crate) fn finish_boot_restore(
    result: Result<Option<SessionInfo>, String>,
    enter_shell: impl FnOnce(SessionInfo),
    show_login: impl FnOnce(Option<String>),
) {
    match result {
        Ok(Some(session)) => enter_shell(session),
        Ok(None) => show_login(None),
        Err(error) => show_login(Some(error)),
    }
}

/// Restore a previously saved session from the encrypted token store.
///
/// Returns `Ok(Some(SessionInfo))` when a saved token is valid and the
/// session is activated, `Ok(None)` when there is no token. A token that
/// exists but is explicitly rejected by Qobuz is cleared and treated as
/// `None`; on network-class failures the token is kept so the session can
/// still be restored later (offline boot, D2 recovery).
pub async fn restore_saved_session<A>(
    runtime: &Arc<AppRuntime<A>>,
) -> Result<Option<SessionInfo>, String>
where
    A: FrontendAdapter + Send + Sync + 'static,
{
    let token = match qbz_credentials::load_oauth_token() {
        Ok(Some(token)) => token,
        Ok(None) => return Ok(None),
        Err(e) => {
            log::warn!("[qbz-qt] could not read saved token: {e}");
            return Ok(None);
        }
    };
    // Same redaction registration as the browser login path.
    qbz_log::register_secret(token.clone());

    let core = runtime.core();
    ensure_api_initialized(core).await?;

    match validated_saved_session(core.login_with_token(&token).await, || {
        let _ = qbz_credentials::clear_oauth_token();
    })? {
        Some(session) => {
            let user_id = session.user_id;
            let display_name = session.display_name.clone();
            let subscription = session.subscription_label.clone();
            crate::local_service_qt::quiesce().await;
            core.set_session(session).await.map_err(|e| e.to_string())?;
            runtime.activate(user_id).await?;
            // Per-user stores the shell depends on — the same binding the
            // fresh-login path uses.
            if let Some(dir) = crate::offline_fwd::user_data_dir(user_id) {
                crate::local_service_qt::bind(&dir);
                crate::offline_fwd::init_for_user(&dir);
                // Plex settings live per-user (plex_settings.db); bind the
                // store to this session so the first Local Library read does
                // not have to.
                crate::local_plex::init_for_user(&dir);
                // Session persistence — see the fresh-login path.
                qbz_app::session_persist::init_for_user(&dir);
                // Listen log (per-user listen_log.db) — opens async; rows start
                // once the store is bound.
                crate::listen_log_qt::init_for_user(&dir);
                crate::library_qt::init_local_favorites(&dir);
                // Qobuz favourite-id cache — see the fresh-login path.
                crate::fav_cache_qt::init_for_user(&dir);
                crate::sidebar_qt::init_pinned(&dir);
                // Phase 15: intelligent-search service (cortinilla
                // cache+ranking).
                crate::search_qt::init(&dir, crate::search_qt::intelligent_search_pref());
                crate::search_cache_qt::init();
                // Phase 17: playlist is_owner math.
                crate::playlist_qt::set_user_id(user_id);
                bind_per_user_stores(&dir, user_id).await;
            }
            crate::offline_fwd::engine().set_offline_session(false);
            crate::media_sync_qt::resume_jellyfin_quality();
            log::info!("[qbz-qt] restored saved session for user {user_id}");
            Ok(Some(SessionInfo {
                display_name,
                subscription,
            }))
        }
        None => Ok(None),
    }
}

/// "Start offline": activate an unauthenticated offline session at the last
/// known user (guest profile user 0 when none — `activate_offline` resolves
/// the same id internally), bind the offline-mode engine, and flag the
/// session-scoped offline state (D1). Returns the resolved user id.
///
pub async fn start_offline_session<A>(runtime: &Arc<AppRuntime<A>>) -> Result<u64, String>
where
    A: FrontendAdapter + Send + Sync + 'static,
{
    let user_id = UserDataPaths::load_last_user_id().unwrap_or(0);
    if user_id == 0 {
        log::info!(
            "[qbz-qt] offline shell: no previous session — entering the guest profile (user 0)"
        );
    }

    // Session scaffolding at the last user (session store, runtime state).
    crate::local_service_qt::quiesce().await;
    runtime.activate_offline().await?;

    // Offline-MODE engine: bind the per-user stores and flag the
    // unauthenticated offline session.
    if let Some(dir) = crate::offline_fwd::user_data_dir(user_id) {
        crate::local_service_qt::bind(&dir);
        crate::offline_fwd::init_for_user(&dir);
        // Plex settings live per-user (plex_settings.db); bind the store to
        // this session so the first Local Library read does not have to.
        crate::local_plex::init_for_user(&dir);
        // Session persistence (queue + current track + position). The store is
        // per-user (`session.db`) and the gates are seeded from this user's
        // playback prefs inside init_for_user, so it works before Settings has
        // published anything.
        qbz_app::session_persist::init_for_user(&dir);
        // Listen log (per-user listen_log.db) — opens async; rows start
        // once the store is bound.
        crate::listen_log_qt::init_for_user(&dir);
        // Phase 5: local-favorites store (Library show-local + hearts).
        crate::library_qt::init_local_favorites(&dir);
        // Qobuz favourite-id cache. The disk seed is the ONLY source offline
        // (no warm can run), which is exactly the gap the reference closes
        // here too — see `fav_cache::init_for_user` on the offline entry.
        crate::fav_cache_qt::init_for_user(&dir);
        // Phase 7: pinned-items store (AlbumCard pin badges).
        crate::sidebar_qt::init_pinned(&dir);
        // Phase 15: intelligent-search service (cortinilla cache+ranking).
        crate::search_qt::init(&dir, crate::search_qt::intelligent_search_pref());
        crate::search_cache_qt::init();
        // Phase 17: playlist is_owner math.
        crate::playlist_qt::set_user_id(user_id);
        // NOT optional on the offline path — see the helper's doc comment for
        // the leak this closes.
        bind_per_user_stores(&dir, user_id).await;
    }
    crate::offline_fwd::engine().set_offline_session(true);
    Ok(user_id)
}

/// Log out: clear the saved token, deactivate the per-user session, drop
/// the Qobuz client session, and tear down the offline-mode per-user state
/// (which reopens the Qobuz gate so a logged-out user can sign back in).
///
/// Every store opened above must be dropped here, and that is not cosmetic:
/// each holds the PREVIOUS user's data in a process-global, so skipping one
/// means the next account inherits it. The favourite-id cache holds their
/// hearts; MyQBZ holds their mixtape grids, the open collection's items, both
/// item caches and the selection set; the add-picker holds the pending batch
/// plus their collection list; the blacklist holds their blocked artists and
/// albums; reco-dismiss holds their "not interested" set.
///
pub async fn logout<A>(runtime: &Arc<AppRuntime<A>>) -> Result<(), String>
where
    A: FrontendAdapter + Send + Sync + 'static,
{
    crate::local_service_qt::reset();
    crate::media_sync_qt::cancel_all();
    let _ = qbz_credentials::clear_oauth_token();
    let _ = runtime.core().logout().await;
    crate::fav_cache_qt::teardown();
    crate::library_qt::teardown();
    crate::sidebar_qt::teardown();
    crate::local_plex::reset();
    crate::playlist_qt::teardown();
    crate::integrations_qt::unbind_qobuz_user();
    // Listen log: close the row in flight and drop the per-user binding.
    crate::listen_log_qt::teardown().await;
    // Intelligent Search: the LEARNED ranking is per-user. Without this the
    // next account inherits a stranger's most-clicked results as its promoted
    // top result, and keeps writing into their buckets.
    crate::search_qt::teardown();
    // MyQBZ: grids + branding/view-prefs binding, then the detail page's
    // caches and the two modal documents.
    crate::myqbz_qt::teardown();
    crate::myqbz_prefs_qt::teardown();
    crate::myqbz_detail_qt::teardown();
    crate::myqbz_edit_qt::teardown();
    crate::myqbz_mix_qt::teardown();
    // The app-wide "Add to Mixtape/Collection" picker: its row cache holds the
    // previous account's collection ids, which a pick would write into.
    crate::myqbz_add_qt::teardown();
    // The Artist-Collection builder holds a whole discography plus the artist
    // id it was opened for.
    crate::myqbz_builder_qt::teardown();
    crate::artist_blacklist::teardown();
    crate::reco_dismiss_qt::teardown();
    // Every source's per-user handle and cache: the pooled `library.db`
    // handles, the Plex art memo, the ephemeral session store. Without this
    // the next account inherits the previous one's handles out of a `'static`
    // registry.
    let jellyfin_gate = crate::media_sync_qt::media_server_state_guard(
        qbz_app::settings::media_servers::MediaServerKind::Jellyfin,
    );
    let subsonic_gate = crate::media_sync_qt::media_server_state_guard(
        qbz_app::settings::media_servers::MediaServerKind::Subsonic,
    );
    crate::media_servers_qt::reset();
    crate::source_wiring::teardown();
    drop(subsonic_gate);
    drop(jellyfin_gate);
    crate::offline_fwd::teardown();
    runtime.deactivate().await?;
    log::info!("[qbz-qt] logged out");
    Ok(())
}





#[cfg(test)]
mod tests {
    use super::*;


    #[test]
    fn only_explicit_rejection_clears_token_and_valid_restore_enters_once() {
        use std::cell::Cell;
        for error in [
            qbz_qobuz::ApiError::AuthenticationError("fixture 401".into()),
            qbz_qobuz::ApiError::IneligibleUser,
        ] {
            let cleared = Cell::new(0);
            let result = validated_saved_session(Err(qbz_core::CoreError::Api(error)), || {
                cleared.set(cleared.get() + 1)
            });
            assert_eq!(cleared.get(), 1);
            assert!(result.unwrap().is_none());
        }
        let session = qbz_qobuz::auth::parse_login_response(&serde_json::json!({
            "user_auth_token": "fixture", "user": {"id": 42}
        }))
        .unwrap();
        let result = validated_saved_session(Ok(session), || panic!("valid token cleared"))
            .unwrap()
            .unwrap();
        assert_eq!(result.user_id, 42);
        let entries = Cell::new(0);
        finish_boot_restore(
            Ok(Some(SessionInfo {
                display_name: "fixture".into(),
                subscription: "Member".into(),
            })),
            |_| entries.set(entries.get() + 1),
            |_| panic!("valid restore went to login"),
        );
        assert_eq!(entries.get(), 1);
        finish_boot_restore(
            Ok(None),
            |_| panic!("absent token activated a session"),
            |error| assert!(error.is_none()),
        );
    }





    #[tokio::test]
    async fn network_failure_preserves_token_and_releases_splash_to_login_offline() {
        use std::cell::{Cell, RefCell};
        qbz_app::ensure_crypto_provider();
        // A real reqwest transport timeout, with no token/keyring/profile I/O.
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let stalled = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut buffer = [0; 1024];
            while socket.read(&mut buffer).await.unwrap_or(0) != 0 {}
        });
        let error = reqwest::Client::new()
            .post(format!("http://{address}/restore"))
            .timeout(Duration::from_millis(100))
            .send()
            .await
            .unwrap_err();
        assert!(error.is_timeout());
        let cleared = Cell::new(false);
        let result = validated_saved_session(Err(qbz_core::CoreError::Api(error.into())), || {
            cleared.set(true)
        });
        assert!(!cleared.get());
        let screen = RefCell::new("splash");
        let message = RefCell::new(None);
        let activated = Cell::new(0);
        finish_boot_restore(
            result.map(|session| session.map(|_| unreachable!())),
            |_| activated.set(activated.get() + 1),
            |error| {
                *screen.borrow_mut() = "login";
                *message.borrow_mut() = error;
            },
        );
        assert_eq!(*screen.borrow(), "login");
        assert!(message
            .borrow()
            .as_ref()
            .is_some_and(|text: &String| !text.is_empty()));
        assert_eq!(activated.get(), 0);
        tokio::time::timeout(Duration::from_secs(1), stalled)
            .await
            .unwrap()
            .unwrap();
        // LoginScreen is the existing route containing Start offline. This
        // controller change neither creates a new mode nor hides that action.
    }
}
