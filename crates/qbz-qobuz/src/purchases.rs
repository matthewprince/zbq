//! Qobuz purchase HTTP methods (Slice 2 of the Purchases port).
//!
//! Ported 1:1 from the Tauri reference at `src-tauri/src/api/client.rs`
//! (`get_user_purchases_page_typed` 1538, `get_user_purchases_ids_page_typed`
//! 1570, `get_user_purchases_all` 1612, `get_user_purchases_all_typed` 1683,
//! `get_track_file_url_by_format` 1768) plus `download_audio_to_path` from
//! `src-tauri/src/commands_v2/helpers.rs:332`.
//!
//! Behavioral divergence from the Tauri reference, intentional and correct:
//!
//! - Every purchase service request routes through `self.http()?` (the in-tree
//!   offline choke point) instead of the raw `self.http` the Tauri build uses.
//!   Purchases therefore fail fast offline (`ApiError::OfflineMode`), consistent
//!   with the rest of `qbz-qobuz`. The Tauri build had no shared offline gate.
//! - The CDN cross-feature gate (`CDN_STREAMING_ACTIVE`) is `src-tauri`-only; it
//!   guarded `download_audio_to_path` against concurrent streaming-vs-download CDN rate
//!   limiting. There is no equivalent shared counter in the Slint stack, so the
//!   ported `download_audio_to_path` has NO playback-vs-download collision protection.
//!   This is a documented limitation, NOT a 1:1 download-function gap — a true
//!   cross-frontend gate is a separate hardening item. Do NOT add a total
//!   request timeout "to be safe": large hi-res downloads legitimately exceed
//!   any fixed budget and the connect-timeout already bounds the dial phase.
//! - `download_audio_to_path` omits the reference's `.use_native_tls()` — this crate is
//!   rustls-only (no `native-tls` feature), matching `cmaf.rs::build_cdn_client`.
//!   `http1_only()` (the RST_STREAM/EOF fix) is retained.

use serde_json::Value;

use super::client::QobuzClient;
use super::endpoints::{self, paths};
use super::error::{ApiError, Result};
use qbz_models::{
    Album, PurchaseAlbum, PurchaseIdsResponse, PurchaseResponse, PurchaseTrack, SearchResultsPage,
    Track,
};

/// What the purchases pagination loop should do after receiving a page.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PurchasePageStep {
    /// Stop; every item has been collected (or the page came back empty).
    Stop,
    /// Request the next page starting at this offset.
    Continue { next_offset: u32 },
}

/// The purchases pagination loop's terminating condition, extracted so it can be
/// asserted rather than merely inspected (contract §12-2).
///
/// Verbatim from the reference (`src-tauri/src/api/client.rs:1694-1729`): `total`
/// is captured only on the FIRST page, and the loop stops when the page came back
/// empty or when `offset + got >= total`.
///
/// The case worth naming is `total == 0` with a non-empty page: `0 + got >= 0`
/// holds immediately, so the loop stops after ONE page. That is not a
/// "truncation at 500" — the caller then reports that page's actual length as the
/// total. The behaviour is replicated because the reference shipped it and the
/// endpoint is untestable from here; the caller logs a warning so the symptom is
/// diagnosable if it ever bites.
pub fn purchase_page_step(offset: u32, got: u32, total: u32) -> PurchasePageStep {
    if got == 0 || offset.saturating_add(got) >= total {
        PurchasePageStep::Stop
    } else {
        PurchasePageStep::Continue {
            next_offset: offset + got,
        }
    }
}

impl QobuzClient {
    /// `/album/get` for the PURCHASE path — header-auth, **UNSIGNED**.
    ///
    /// The general `get_album` signs this call. Signing evidently works (QBZ
    /// browses the catalogue with it every day), so this is not a bug being
    /// fixed — but on the purchase path it is a variable nobody can test, and it
    /// diverges from both implementations known to work there. Neither the
    /// reference (`src-tauri/src/api/client.rs:641`) nor Qobuz's own Electron
    /// desktop client sign it: in the vendor bundle the request helper takes a
    /// SIGN flag, and `album/get`, `track/get` and `purchase/getUserPurchases`
    /// all pass it false while `track/getFileUrl` passes it true (contract
    /// §2.1b). So the purchase path matches the vendor exactly.
    ///
    /// Unsigned is NOT anonymous: measured 2026-08-15, `/album/get` with
    /// `X-App-Id` alone returns **401**. The user token is required, which is
    /// what `authenticated_headers()` supplies.
    ///
    /// Status IS hard-checked here, unlike the two purchases list endpoints. That
    /// asymmetry is the reference's (`client.rs:653-669`) and it is load-bearing:
    /// this response is parsed with the STRICT `Album`/`Track` structs, whose
    /// optional fields carry no lenient wrapper, so one wrong-typed value fails
    /// the whole album rather than degrading to an empty page.
    pub async fn get_album_for_purchase(&self, album_id: &str) -> Result<Album> {
        let url = endpoints::build_url(paths::ALBUM_GET);
        let http_response = self
            .http()?
            .get(&url)
            .headers(self.authenticated_headers().await?)
            .query(&[("album_id", album_id)])
            .send()
            .await?;
        let status = http_response.status();
        log::debug!("[Purchases] get_album_for_purchase({album_id}) status={status}");

        if status == reqwest::StatusCode::NOT_FOUND {
            return Err(ApiError::ApiResponse(format!(
                "Album {album_id} not found (404)"
            )));
        }
        if !status.is_success() {
            return Err(ApiError::ApiResponse(format!(
                "get_album_for_purchase({album_id}) status {status}"
            )));
        }

        let response: Value = http_response.json().await?;
        Ok(serde_json::from_value(response)?)
    }

    /// `/track/get` for the PURCHASE path — header-auth, **UNSIGNED**.
    ///
    /// Same rationale as `get_album_for_purchase`. This call runs before EVERY
    /// purchase download (`legacy_compat.rs:2659` calls `get_track` immediately
    /// before `get_track_file_url_by_format`) and supplies the artist, album
    /// title and track number that build the on-disk path — and, since the scope
    /// expansion, the metadata written into the file's tags.
    ///
    /// It carries no entitlement. `/track/getFileUrl` is where the right to the
    /// file is proved, by its signature, and that call is untouched.
    pub async fn get_track_for_purchase(&self, track_id: u64) -> Result<Track> {
        let url = endpoints::build_url(paths::TRACK_GET);
        let http_response = self
            .http()?
            .get(&url)
            .headers(self.authenticated_headers().await?)
            .query(&[("track_id", track_id.to_string())])
            .send()
            .await?;
        let status = http_response.status();
        log::debug!("[Purchases] get_track_for_purchase({track_id}) status={status}");

        if status == reqwest::StatusCode::NOT_FOUND {
            return Err(ApiError::TrackUnavailable(track_id));
        }
        if !status.is_success() {
            return Err(ApiError::ApiResponse(format!(
                "get_track_for_purchase({track_id}) status {status}"
            )));
        }

        let response: Value = http_response.json().await?;
        Ok(serde_json::from_value(response)?)
    }

    /// Get one purchases page from Qobuz, optionally constrained by purchase
    /// type (`"albums"` / `"tracks"`; omitted if `None`).
    ///
    /// Header-auth, UNSIGNED — requires login (`authenticated_headers`).
    /// Ported from `src-tauri/src/api/client.rs:1538`.
    pub async fn get_user_purchases_page_typed(
        &self,
        purchase_type: Option<&str>,
        limit: u32,
        offset: u32,
    ) -> Result<PurchaseResponse> {
        let url = endpoints::build_url(paths::PURCHASE_GET_USER_PURCHASES);
        let mut query: Vec<(&str, String)> =
            vec![("limit", limit.to_string()), ("offset", offset.to_string())];
        if let Some(kind) = purchase_type {
            query.push(("type", kind.to_string()));
        }

        let http_response = self
            .http()?
            .get(&url)
            .headers(self.authenticated_headers().await?)
            .query(&query)
            .send()
            .await?;
        log::debug!(
            "[Purchases] get_user_purchases_page(type={:?}, limit={}, offset={}) status={}",
            purchase_type,
            limit,
            offset,
            http_response.status()
        );
        let response: Value = http_response.json().await?;
        Ok(serde_json::from_value(response)?)
    }

    /// Fail-closed variant used for entitlement decisions. Unlike the legacy
    /// Purchases list loader, an HTTP error is never allowed to deserialize as
    /// an empty page and masquerade as a fresh "not owned" answer.
    pub async fn get_user_purchases_page_typed_checked(
        &self,
        purchase_type: Option<&str>,
        limit: u32,
        offset: u32,
    ) -> Result<PurchaseResponse> {
        let url = endpoints::build_url(paths::PURCHASE_GET_USER_PURCHASES);
        let mut query: Vec<(&str, String)> =
            vec![("limit", limit.to_string()), ("offset", offset.to_string())];
        if let Some(kind) = purchase_type {
            query.push(("type", kind.to_string()));
        }

        let http_response = self
            .http()?
            .get(&url)
            .headers(self.authenticated_headers().await?)
            .query(&query)
            .send()
            .await?;
        let status = http_response.status();
        log::debug!(
            "[Purchases] checked entitlement page(type={:?}, limit={}, offset={}) status={}",
            purchase_type,
            limit,
            offset,
            status
        );
        if !status.is_success() {
            return Err(ApiError::ApiResponse(format!(
                "checked purchase entitlement request failed with status {status}"
            )));
        }
        let response: Value = http_response.json().await?;
        Ok(serde_json::from_value(response)?)
    }

    /// Get one purchases-ids page from Qobuz, optionally constrained by purchase
    /// type. The items are OPAQUE — the UI reads only `.total` per type.
    ///
    /// Header-auth, UNSIGNED. `getUserPurchasesIds` is NOT in the OpenAPI spec;
    /// the code is the source of truth for its `{albums:{...}, tracks:{...}}`
    /// envelope. Ported from `src-tauri/src/api/client.rs:1570`.
    pub async fn get_user_purchases_ids_page_typed(
        &self,
        purchase_type: Option<&str>,
        limit: u32,
        offset: u32,
    ) -> Result<PurchaseIdsResponse> {
        let url = endpoints::build_url(paths::PURCHASE_GET_USER_PURCHASES_IDS);
        let mut query: Vec<(&str, String)> =
            vec![("limit", limit.to_string()), ("offset", offset.to_string())];
        if let Some(kind) = purchase_type {
            query.push(("type", kind.to_string()));
        }

        let http_response = self
            .http()?
            .get(&url)
            .headers(self.authenticated_headers().await?)
            .query(&query)
            .send()
            .await?;
        log::debug!(
            "[Purchases] get_user_purchases_ids_page(type={:?}, limit={}, offset={}) status={}",
            purchase_type,
            limit,
            offset,
            http_response.status()
        );
        let response: Value = http_response.json().await?;
        Ok(serde_json::from_value(response)?)
    }

    /// Get all purchases by paginating through the Qobuz purchases API.
    ///
    /// Paginates `"albums"` then `"tracks"` separately (`page_limit = 500`); per
    /// type reads `.total` on the first page, accumulates, and breaks when
    /// `got == 0` OR `offset + got >= total`. Final totals fall back to
    /// `items.len()` when the server total was 0. Returns offset=0, limit=500 on
    /// both pages. Ported from `src-tauri/src/api/client.rs:1612`.
    pub async fn get_user_purchases_all(&self) -> Result<PurchaseResponse> {
        let page_limit = 500u32;
        let mut all_albums: Vec<PurchaseAlbum> = Vec::new();
        let mut all_tracks: Vec<PurchaseTrack> = Vec::new();
        let mut albums_total = 0u32;
        let mut tracks_total = 0u32;

        let mut albums_offset = 0u32;
        loop {
            let page = self
                .get_user_purchases_page_typed(Some("albums"), page_limit, albums_offset)
                .await?;
            if albums_offset == 0 {
                albums_total = page.albums.total;
            }

            let got = page.albums.items.len() as u32;
            all_albums.extend(page.albums.items);

            if got == 0 || albums_offset + got >= albums_total {
                break;
            }
            albums_offset += got;
        }

        let mut tracks_offset = 0u32;
        loop {
            let page = self
                .get_user_purchases_page_typed(Some("tracks"), page_limit, tracks_offset)
                .await?;
            if tracks_offset == 0 {
                tracks_total = page.tracks.total;
            }

            let got = page.tracks.items.len() as u32;
            all_tracks.extend(page.tracks.items);

            if got == 0 || tracks_offset + got >= tracks_total {
                break;
            }
            tracks_offset += got;
        }

        let final_albums_total = if albums_total == 0 {
            all_albums.len() as u32
        } else {
            albums_total
        };
        let final_tracks_total = if tracks_total == 0 {
            all_tracks.len() as u32
        } else {
            tracks_total
        };

        Ok(PurchaseResponse {
            albums: SearchResultsPage {
                items: all_albums,
                total: final_albums_total,
                offset: 0,
                limit: page_limit,
            },
            tracks: SearchResultsPage {
                items: all_tracks,
                total: final_tracks_total,
                offset: 0,
                limit: page_limit,
            },
        })
    }

    /// Get all purchases for a single type by paginating through the Qobuz
    /// purchases API.
    ///
    /// Same loop as `get_user_purchases_all` but for ONE type; the OTHER type's
    /// `total` is forced to 0 in the returned envelope (the root of the per-type
    /// totals gotcha — the controller must call `get_user_purchases_ids_page_typed`
    /// separately per type to recover both totals). Unsupported type →
    /// `ApiError::ApiResponse("Unsupported purchase type: {}")`. Ported from
    /// `src-tauri/src/api/client.rs:1683`.
    pub async fn get_user_purchases_all_typed(
        &self,
        purchase_type: &str,
    ) -> Result<PurchaseResponse> {
        // Pre-flight validation (contract §8-D2). The reference validates the
        // type BEFORE any I/O (`legacy_compat.rs:2773-2778`); this loop used to
        // fetch a page first and only reject inside the match arm, so a bogus
        // type issued a live authenticated GET before failing. Same error, no
        // request.
        if purchase_type != "albums" && purchase_type != "tracks" {
            return Err(ApiError::ApiResponse(format!(
                "Unsupported purchase type: {}",
                purchase_type
            )));
        }

        let page_limit = 500u32;
        let mut offset = 0u32;

        let mut all_albums: Vec<PurchaseAlbum> = Vec::new();
        let mut all_tracks: Vec<PurchaseTrack> = Vec::new();
        let mut total = 0u32;

        loop {
            let page = self
                .get_user_purchases_page_typed(Some(purchase_type), page_limit, offset)
                .await?;

            match purchase_type {
                "albums" => {
                    if offset == 0 {
                        total = page.albums.total;
                    }
                    let got = page.albums.items.len() as u32;
                    all_albums.extend(page.albums.items);
                    match purchase_page_step(offset, got, total) {
                        PurchasePageStep::Stop => break,
                        PurchasePageStep::Continue { next_offset } => offset = next_offset,
                    }
                }
                "tracks" => {
                    if offset == 0 {
                        total = page.tracks.total;
                    }
                    let got = page.tracks.items.len() as u32;
                    all_tracks.extend(page.tracks.items);
                    match purchase_page_step(offset, got, total) {
                        PurchasePageStep::Stop => break,
                        PurchasePageStep::Continue { next_offset } => offset = next_offset,
                    }
                }
                // Unreachable: the type is validated before the first request.
                // Kept so the match stays exhaustive without an unreachable!().
                _ => break,
            }
        }

        // §2.3: a ZERO server total terminates the loop after a single page,
        // because `offset + got >= total` holds immediately. The returned total
        // then falls back to the accumulated length — which is THAT PAGE's actual
        // length, not a 500-item truncation. Replicated verbatim because the
        // reference shipped it, but it is worth a log line: a user whose library
        // silently stops at one page has no other signal, and without this the
        // symptom gets misdiagnosed as "truncated at 500".
        let accumulated = if purchase_type == "albums" {
            all_albums.len() as u32
        } else {
            all_tracks.len() as u32
        };
        if total == 0 && accumulated > 0 {
            log::warn!(
                "[Purchases] {purchase_type}: server reported total=0 while returning \
                 {accumulated} item(s); pagination stopped after the first page. \
                 Reporting total={accumulated}. If the real library is larger, this \
                 is where the items were lost."
            );
        }

        let final_total = if total == 0 { accumulated } else { total };

        Ok(PurchaseResponse {
            albums: SearchResultsPage {
                items: all_albums,
                total: if purchase_type == "albums" {
                    final_total
                } else {
                    0
                },
                offset: 0,
                limit: page_limit,
            },
            tracks: SearchResultsPage {
                items: all_tracks,
                total: if purchase_type == "tracks" {
                    final_total
                } else {
                    0
                },
                offset: 0,
                limit: page_limit,
            },
        })
    }

    // Purchase downloads (`track/getFileUrl` with `intent=download`, signed
    // with Qobuz's app secret) were removed. Download purchases from qobuz.com
    // and import them as local files.

}

#[cfg(test)]
mod purchase_contract_tests {
    use super::*;

    // The purchase listing endpoints must match the reference paths exactly.

    /// The endpoint constants themselves — a typo here fails silently into an
    /// empty purchases list, because the list endpoints never check status.
    #[test]
    fn purchase_endpoint_paths_are_the_reference_paths() {
        assert_eq!(
            paths::PURCHASE_GET_USER_PURCHASES,
            "/purchase/getUserPurchases"
        );
        assert_eq!(
            paths::PURCHASE_GET_USER_PURCHASES_IDS,
            "/purchase/getUserPurchasesIds"
        );
        assert_eq!(paths::ALBUM_GET, "/album/get");
        assert_eq!(paths::TRACK_GET, "/track/get");
    }

    // ── §12-2: the pagination loop's terminating condition ───────────────────

    #[test]
    fn pagination_stops_on_an_empty_page() {
        assert_eq!(purchase_page_step(0, 0, 1_000), PurchasePageStep::Stop);
        // Even mid-walk, and even when the server claims more remain.
        assert_eq!(purchase_page_step(500, 0, 1_000), PurchasePageStep::Stop);
    }

    #[test]
    fn pagination_walks_while_items_remain() {
        assert_eq!(
            purchase_page_step(0, 500, 1_200),
            PurchasePageStep::Continue { next_offset: 500 }
        );
        assert_eq!(
            purchase_page_step(500, 500, 1_200),
            PurchasePageStep::Continue { next_offset: 1_000 }
        );
        // Final page: 1000 + 200 >= 1200.
        assert_eq!(
            purchase_page_step(1_000, 200, 1_200),
            PurchasePageStep::Stop
        );
    }

    /// THE case from §2.3. A zero total stops the walk after one page even
    /// though that page was full. The caller then reports the accumulated
    /// length, so the symptom is "my library is exactly one page long", not
    /// "truncated at 500" — a distinction that has to survive in a test because
    /// it cannot be reproduced against a live account here.
    #[test]
    fn pagination_zero_total_stops_after_one_page_even_when_full() {
        assert_eq!(purchase_page_step(0, 500, 0), PurchasePageStep::Stop);
        assert_eq!(purchase_page_step(0, 1, 0), PurchasePageStep::Stop);
    }

    /// Exact-boundary and overshoot: `>=`, never `>`.
    #[test]
    fn pagination_boundary_is_inclusive() {
        assert_eq!(purchase_page_step(0, 500, 500), PurchasePageStep::Stop);
        assert_eq!(purchase_page_step(0, 501, 500), PurchasePageStep::Stop);
        assert_eq!(
            purchase_page_step(0, 499, 500),
            PurchasePageStep::Continue { next_offset: 499 }
        );
    }
}
