//! Login-response parsing and small time helpers.
//!
//! Upstream QBZ's request signing (`generate_signature`, `sign_*`, the CMAF
//! seed) was removed: this client never signs requests and never holds a Qobuz
//! secret. See the crate docs and COMPLIANCE.md.

use chrono::{TimeZone, Utc};
use std::time::{SystemTime, UNIX_EPOCH};

use super::error::{ApiError, Result};
use qbz_models::{Entitlements, UserSession};

/// Get current Unix timestamp
pub fn get_timestamp() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("Time went backwards")
        .as_secs()
}

/// Parse user login response
pub fn parse_login_response(response: &serde_json::Value) -> Result<UserSession> {
    let user = response
        .get("user")
        .ok_or_else(|| ApiError::AuthenticationError("No user in response".to_string()))?;

    let user_auth_token = response
        .get("user_auth_token")
        .and_then(|v| v.as_str())
        .ok_or_else(|| ApiError::AuthenticationError("No auth token in response".to_string()))?
        .to_string();

    let user_id = user
        .get("id")
        .and_then(|v| v.as_u64())
        .ok_or_else(|| ApiError::AuthenticationError("No user id".to_string()))?;

    let email = user
        .get("email")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();

    let display_name = user
        .get("display_name")
        .and_then(|v| v.as_str())
        .or_else(|| user.get("login").and_then(|v| v.as_str()))
        .unwrap_or("")
        .to_string();

    // Subscription / entitlements. `credential.parameters` is the server's
    // verdict: a populated object for a subscriber, `null`/absent/empty
    // for a Qobuz member without a subscription. The member is a valid
    // session (member mode, 2026-09-02): favorites, playlists, purchases
    // and previews all work; the flags below say what does not.
    let credential = user.get("credential");
    let parameters = credential
        .and_then(|c| c.get("parameters"))
        .and_then(|p| p.as_object())
        .filter(|o| !o.is_empty());
    let entitlements = parameters
        .map(|p| Entitlements {
            lossy_streaming: flag(p, "lossy_streaming"),
            lossless_streaming: flag(p, "lossless_streaming"),
            hires_streaming: flag(p, "hires_streaming"),
            hires_purchases_streaming: flag(p, "hires_purchases_streaming"),
            offline_streaming: flag(p, "offline_streaming"),
            mobile_streaming: flag(p, "mobile_streaming"),
        })
        .unwrap_or_default();
    let subscription_label = match parameters {
        Some(p) => p
            .get("short_label")
            .and_then(|v| v.as_str())
            .unwrap_or("Unknown")
            .to_string(),
        // msgid; the UI translates it (the web player says "Qobuz member").
        None => "Member".to_string(),
    };
    let subscription = user.get("subscription");
    let subscription_end_date = subscription
        .and_then(|s| s.get("end_date"))
        .and_then(|v| v.as_str())
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string);
    let subscription_offer = subscription
        .and_then(|s| s.get("offer"))
        .and_then(|v| v.as_str())
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string);

    fn parse_subscription_valid_until(parameters: &serde_json::Value) -> Option<String> {
        // Try common string fields first.
        let string_keys = [
            "end_date",
            "expiration_date",
            "valid_until",
            "expires_at",
            "expiry_date",
        ];
        for key in string_keys {
            if let Some(s) = parameters.get(key).and_then(|v| v.as_str()) {
                let trimmed = s.trim();
                if !trimmed.is_empty() {
                    return Some(trimmed.to_string());
                }
            }
        }

        // Try common timestamp fields (seconds).
        let ts_keys = [
            "end_date_ts",
            "expires_at_ts",
            "expiration_ts",
            "valid_until_ts",
        ];
        for key in ts_keys {
            if let Some(ts) = parameters.get(key).and_then(|v| v.as_i64()) {
                if ts > 0 {
                    return Some(Utc.timestamp_opt(ts, 0).single()?.date_naive().to_string());
                }
            }
        }

        None
    }

    let subscription_valid_until = credential
        .and_then(|c| c.get("parameters"))
        .and_then(parse_subscription_valid_until);

    // Account territory + language (snake_case wire names, verbatim in
    // Qobuz's own embedded /user/login fixture — see qbz-nix-docs
    // offline-mode/tauri-review-2026-06-09/10-subscription-trial-offline-
    // gating.md §1.2). Absent on older captures -> None (feature stays off).
    let country_code = user
        .get("country_code")
        .and_then(|v| v.as_str())
        .map(str::to_string);
    let language_code = user
        .get("language_code")
        .and_then(|v| v.as_str())
        .map(str::to_string);

    Ok(UserSession {
        user_auth_token,
        user_id,
        email,
        display_name,
        subscription_label,
        subscription_valid_until,
        entitlements,
        subscription_end_date,
        subscription_offer,
        country_code,
        language_code,
    })
}

/// A boolean entitlement flag; anything but literal `true` is `false`.
fn flag(parameters: &serde_json::Map<String, serde_json::Value>, key: &str) -> bool {
    parameters
        .get(key)
        .and_then(|v| v.as_bool())
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn login_response(user_extra: serde_json::Value) -> serde_json::Value {
        let mut user = serde_json::json!({
            "id": 1705826,
            "email": "a@b.c",
            "display_name": "Tester",
            "credential": {"parameters": {"short_label": "Studio"}}
        });
        user.as_object_mut()
            .unwrap()
            .extend(user_extra.as_object().unwrap().clone());
        serde_json::json!({
            "user_auth_token": "token",
            "user": user,
        })
    }

    #[test]
    fn parse_login_response_captures_country_and_language() {
        let response = login_response(serde_json::json!({
            "country_code": "FR",
            "language_code": "fr",
        }));
        let session = parse_login_response(&response).expect("valid login response");
        assert_eq!(session.country_code.as_deref(), Some("FR"));
        assert_eq!(session.language_code.as_deref(), Some("fr"));
    }

    #[test]
    fn parse_login_response_tolerates_missing_country_and_language() {
        // Older captures / partial payloads: both stay None (feature off),
        // the rest of the session parses as before.
        let response = login_response(serde_json::json!({}));
        let session = parse_login_response(&response).expect("valid login response");
        assert_eq!(session.country_code, None);
        assert_eq!(session.language_code, None);
    }

    /// Qobuz's own embedded `/user/login` fixture (webpack module 58981 of
    /// the web player bundle): an internal Studio account with a PAST
    /// `end_date` and `is_canceled: true`, yet every entitlement `true`.
    /// The flags are the verdict; the date is not.
    fn studio_fixture() -> serde_json::Value {
        serde_json::json!({
            "user_auth_token": "CmJsOF3qsokN12dd",
            "user": {
                "id": 1705826,
                "email": "account-test+FRfavorites@qobuz.com",
                "login": "test-FR-Studio",
                "display_name": "test FR Studio",
                "country_code": "FR",
                "language_code": "fr",
                "subscription": {
                    "offer": "studio",
                    "periodicity": "annual",
                    "end_date": "2022-06-19",
                    "is_canceled": true
                },
                "credential": {
                    "id": 1489142,
                    "label": "streaming-studio",
                    "description": "Abonné Qobuz Studio",
                    "parameters": {
                        "lossy_streaming": true,
                        "lossless_streaming": true,
                        "hires_streaming": true,
                        "hires_purchases_streaming": true,
                        "mobile_streaming": true,
                        "offline_streaming": true,
                        "hfp_purchase": false,
                        "included_format_group_ids": [1, 2, 3, 4],
                        "label": "Qobuz Studio",
                        "short_label": "Studio",
                        "source": "internal"
                    }
                }
            }
        })
    }

    #[test]
    fn subscriber_carries_entitlements_offer_and_end_date() {
        let session = parse_login_response(&studio_fixture()).expect("subscriber parses");
        assert_eq!(session.subscription_label, "Studio");
        assert!(session.entitlements.offline_streaming);
        assert!(session.entitlements.hires_streaming);
        assert!(session.entitlements.hires_purchases_streaming);
        assert!(!session.entitlements.is_member_only());
        assert_eq!(session.subscription_offer.as_deref(), Some("studio"));
        assert_eq!(session.subscription_end_date.as_deref(), Some("2022-06-19"));
    }

    /// A Qobuz member without a subscription: Qobuz answers 200 and the
    /// session is real; QBZ used to throw it away (`IneligibleUser`). The
    /// web player shows the same account as "Qobuz member" with its
    /// favorites and playlists (verified 2026-09-02 with a lapsed account).
    #[test]
    fn member_without_parameters_is_a_valid_session() {
        for credential in [
            serde_json::json!({"parameters": null}),
            serde_json::json!({"parameters": {}}),
            serde_json::json!({}),
        ] {
            let response = login_response(serde_json::json!({ "credential": credential }));
            let session = parse_login_response(&response)
                .unwrap_or_else(|e| panic!("member must parse, got {e} for {credential}"));
            assert_eq!(session.user_id, 1705826);
            assert_eq!(session.subscription_label, "Member");
            assert_eq!(session.entitlements, Entitlements::default());
            assert!(session.entitlements.is_member_only());
            assert_eq!(session.subscription_end_date, None);
        }
        // No `credential` object at all: same verdict.
        let mut response = login_response(serde_json::json!({}));
        response["user"]
            .as_object_mut()
            .unwrap()
            .remove("credential");
        let session = parse_login_response(&response).expect("member without credential");
        assert_eq!(session.subscription_label, "Member");
        assert!(session.entitlements.is_member_only());
    }

    #[test]
    fn flags_default_false_when_absent_or_not_boolean() {
        let response = login_response(serde_json::json!({
            "credential": {"parameters": {
                "short_label": "Solo",
                "lossless_streaming": true,
                "hires_streaming": "true",
                "offline_streaming": 1
            }}
        }));
        let session = parse_login_response(&response).expect("parses");
        assert_eq!(session.subscription_label, "Solo");
        assert!(session.entitlements.lossless_streaming);
        assert!(!session.entitlements.hires_streaming);
        assert!(!session.entitlements.offline_streaming);
        assert!(!session.entitlements.is_member_only());
    }

    #[test]
    fn persisted_session_without_new_fields_still_loads() {
        // A session snapshot written before member mode: no entitlements,
        // no subscription fields. Must deserialize with everything false.
        let json = r#"{"user_auth_token":"t","user_id":1,"email":"","display_name":"",
            "subscription_label":"Studio"}"#;
        let session: UserSession = serde_json::from_str(json).expect("old snapshot loads");
        assert_eq!(session.entitlements, Entitlements::default());
        assert_eq!(session.subscription_offer, None);
    }
}
