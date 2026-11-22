// crates/qbzd/src/login.rs — `qbzd login` / `qbzd logout` (02-cli-and-api.md §2.2,
// memo D6).
//
// The only way in is the user's own Qobuz session token (`qbzd login --token`, or
// the desktop app's sign-in carried over with `qbzd settings export --from desktop
// --include-auth` + `import --include-auth`). Upstream also had a browser OAuth
// flow and a pasted-redirect flow; both finished sign-in with a private key taken
// from Qobuz's web player, so they were removed (COMPLIANCE.md).
//
// The command only VALIDATES (live `login_with_token`) and PERSISTS the token into
// the daemon root, then best-effort nudges a running daemon to reload. It never
// activates a session in-process — the daemon (a separate process) owns session
// activation.
//
// There is NO email+password surface anywhere (D6/D12).
use std::io::{Read, Write};
use std::net::{TcpStream, ToSocketAddrs};
use std::time::Duration;

use qbz_app::shell::AppRuntime;
use qbz_audio::settings::AudioSettings;
use qbz_core::NoOpAdapter;
use qbz_models::UserSession;

use crate::paths::ProfileRoots;

/// Everything that can go wrong on the way to a persisted session. Every variant
/// renders with a `→` fix line (02 §1.4). All map to exit 1 in `main` (login
/// never reports 3/4 — it validates and persists locally, so it works daemon-up
/// or daemon-down, §2.2).
#[derive(Debug)]
pub enum LoginError {
    /// `qbzd login` without a token: explain how to sign in.
    HowToSignIn,
    /// Qobuz explicitly rejected the credentials (401 / ineligible account).
    Rejected(String),
    /// Any other local or network-class failure.
    Failed(String),
}

impl std::fmt::Display for LoginError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            LoginError::HowToSignIn => write!(f, "{}", crate::cli::copy::login_how_to()),
            LoginError::Rejected(msg) => write!(
                f,
                "error: Qobuz rejected the credentials ({msg})\n  \
                 → check the token or sign in again:  qbzd login --token <user_auth_token>"
            ),
            LoginError::Failed(msg) => {
                write!(f, "error: {msg}")?;
                if !msg.contains('→') {
                    write!(
                        f,
                        "\n  → check your connection and retry:  qbzd login --token <user_auth_token>"
                    )?;
                }
                Ok(())
            }
        }
    }
}

impl std::error::Error for LoginError {}

// ============================ public entry points ============================

/// Live-validate a raw `user_auth_token` via `login_with_token` BEFORE it is
/// ever persisted. The returned session is the source of truth for the user id
/// and plan. Registers the token as a redaction secret first, so no log line
/// that might carry it (in the client or elsewhere) can leak it.
///
/// T12 (settings import) and T13 (setup TUI Account screen) reuse this.
pub async fn validate_token(token: &str) -> Result<UserSession, LoginError> {
    // §6.3: register before any log line can carry the token.
    qbz_log::register_secret(token.to_string());
    let runtime = build_login_runtime().await?;
    runtime
        .core()
        .login_with_token(token)
        .await
        .map_err(map_core_err)
}

/// `qbzd login --token` (02 §2.2): a directly-injected `user_auth_token`.
/// Validated live, then persisted.
pub async fn login_with_token_arg(
    roots: &ProfileRoots,
    token: &str,
) -> Result<UserSession, LoginError> {
    let session = validate_token(token).await?;
    finalize(roots, &session)?;
    Ok(session)
}

/// `qbzd logout` (02 §2.2): clear the daemon-root credential file and nudge a
/// running daemon into NeedsAuth. Returns whether the daemon acknowledged the
/// reload, so the caller can pick the right success line.
pub fn logout(roots: &ProfileRoots) -> Result<bool, LoginError> {
    qbz_credentials::clear_oauth_token_at(&roots.config)
        .map_err(|e| LoginError::Failed(format!("could not clear the credential file: {e}")))?;
    let host = nudge_host(roots);
    // token: opt-in [server] token, wired by T6.
    Ok(nudge_reload(&host, None))
}

/// Three-state outcome of the ping-then-reload nudge. 04-settings-portability.md
/// §5.3 step 7 needs "daemon simply not running" (not an error) distinguished
/// from "daemon up but the reload was refused/500" (exit 1 with the restart
/// hint) — a single bool conflates them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NudgeOutcome {
    /// Ping answered and the reload returned 2xx.
    Reloaded,
    /// Ping did not answer — no daemon to nudge (never an error).
    DaemonDown,
    /// Ping answered but the reload did not return 2xx.
    ReloadRefused,
}

/// Best-effort `GET /api/ping` → `POST /api/settings/reload` against a local
/// daemon. `token` carries the opt-in `[server] token` as
/// `Authorization: Bearer` when present; T5 callers pass `None`.
pub fn nudge_reload_outcome(host: &str, token: Option<&str>) -> NudgeOutcome {
    if !http_request_2xx(host, "GET", "/api/ping", token) {
        return NudgeOutcome::DaemonDown;
    }
    if http_request_2xx(host, "POST", "/api/settings/reload", token) {
        NudgeOutcome::Reloaded
    } else {
        NudgeOutcome::ReloadRefused
    }
}

/// Boolean skin over [`nudge_reload_outcome`] for the callers that only need
/// "did a running daemon acknowledge?" (login/logout/`settings set` — they are
/// specified to work daemon-down, 02 §2.2, so any non-reload is just "the
/// daemon picks it up on next start").
pub fn nudge_reload(host: &str, token: Option<&str>) -> bool {
    nudge_reload_outcome(host, token) == NudgeOutcome::Reloaded
}

// ============================ IO helpers ============================

/// Compose the minimal client-only runtime: a headless [`NoOpAdapter`] and
/// default audio settings (no store is opened — login never touches audio).
async fn build_login_runtime() -> Result<AppRuntime<NoOpAdapter>, LoginError> {
    let runtime =
        AppRuntime::with_audio_settings(NoOpAdapter, None, AudioSettings::default(), None);
    if let Err(e) = runtime.init().await {
        return Err(LoginError::Failed(format!(
            "could not reach Qobuz to start login: {e}\n  → check your connection and retry"
        )));
    }
    Ok(runtime)
}

/// Register the secret, persist the token into the daemon config root (0600),
/// then best-effort nudge a running daemon. Persist happens ONLY here — after
/// the caller already live-validated the session.
fn finalize(roots: &ProfileRoots, session: &UserSession) -> Result<(), LoginError> {
    // §6.3: register before the token can reach any log line (idempotent — the
    // token path already registered it in `validate_token`).
    qbz_log::register_secret(session.user_auth_token.clone());
    qbz_credentials::save_oauth_token_at(&roots.config, &session.user_auth_token).map_err(|e| {
        LoginError::Failed(format!(
            "could not save credentials to {}: {e}",
            roots.config.display()
        ))
    })?;
    let host = nudge_host(roots);
    // token: opt-in [server] token, wired by T6.
    let _ = nudge_reload(&host, None);
    Ok(())
}

/// The local daemon's reload address. Credentials are written to the LOCAL
/// config root, so the daemon to nudge is always local; its port comes from the
/// same `qbzd.toml` the daemon reads (default 8182).
pub(crate) fn nudge_host(roots: &ProfileRoots) -> String {
    let port = crate::config::QbzdConfig::load(&roots.config.join("qbzd.toml"))
        .map(|(c, _)| c.server.port)
        .unwrap_or(8182);
    format!("127.0.0.1:{port}")
}

fn http_request_2xx(host: &str, method: &str, path: &str, token: Option<&str>) -> bool {
    let addr = match host.to_socket_addrs().ok().and_then(|mut a| a.next()) {
        Some(a) => a,
        None => return false,
    };
    let mut stream = match TcpStream::connect_timeout(&addr, Duration::from_millis(600)) {
        Ok(s) => s,
        Err(_) => return false,
    };
    let _ = stream.set_read_timeout(Some(Duration::from_secs(2)));
    let _ = stream.set_write_timeout(Some(Duration::from_secs(2)));
    let auth = token
        .map(|t| format!("Authorization: Bearer {t}\r\n"))
        .unwrap_or_default();
    let req = format!(
        "{method} {path} HTTP/1.1\r\nHost: {host}\r\n{auth}Content-Length: 0\r\nConnection: close\r\n\r\n"
    );
    if stream.write_all(req.as_bytes()).is_err() {
        return false;
    }
    let _ = stream.flush();
    let mut buf = [0u8; 128];
    let n = stream.read(&mut buf).unwrap_or(0);
    let status = String::from_utf8_lossy(&buf[..n]);
    matches!(
        status.lines().next().and_then(|l| l.split_whitespace().nth(1)),
        Some(code) if code.starts_with('2')
    )
}

// ============================ error mapping ============================

fn map_core_err(e: qbz_core::CoreError) -> LoginError {
    if matches!(
        e,
        qbz_core::CoreError::Api(
            qbz_qobuz::ApiError::AuthenticationError(_) | qbz_qobuz::ApiError::IneligibleUser
        )
    ) {
        LoginError::Rejected(e.to_string())
    } else {
        LoginError::Failed(e.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::TcpListener;

    #[test]
    fn nudge_reload_is_false_when_daemon_is_down() {
        // Bind then immediately drop to obtain a definitely-closed local port.
        let l = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = l.local_addr().unwrap().port();
        drop(l);
        assert!(!nudge_reload(&format!("127.0.0.1:{port}"), None));
    }

    #[test]
    fn nudge_outcome_is_daemon_down_when_nothing_listens() {
        // The three-state outcome: no listener → DaemonDown (never ReloadRefused).
        let l = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = l.local_addr().unwrap().port();
        drop(l);
        assert_eq!(
            nudge_reload_outcome(&format!("127.0.0.1:{port}"), None),
            NudgeOutcome::DaemonDown
        );
    }

    #[test]
    fn how_to_sign_in_names_both_token_routes() {
        let rendered = LoginError::HowToSignIn.to_string();
        assert!(rendered.contains("qbzd login --token <user_auth_token>"), "{rendered}");
        assert!(rendered.contains("settings export --from desktop --include-auth"), "{rendered}");
        assert!(!rendered.contains("oauth"), "{rendered}");
    }
}
