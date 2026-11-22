//! Qobuz API client (catalog, library and account calls only)
//!
//! This crate talks to Qobuz's catalog and library endpoints with the user's
//! own session token and the official web player's public app ID, exactly as
//! the web player does. It deliberately contains NO request signing, NO app
//! secret, NO stream-URL acquisition and NO stream decryption: those were
//! removed from the upstream QBZ code because obtaining or using Qobuz's
//! signing secret and decrypting its streams outside Qobuz's own player is
//! circumvention of an access control (DMCA §1201, EU InfoSoc Art. 6).
//! Qobuz playback happens inside Qobuz's official web player instead (see
//! COMPLIANCE.md at the repository root). Do not reintroduce any of it.
//!
//! # Usage
//!
//! ```rust,ignore
//! use qbz_qobuz::QobuzClient;
//!
//! let client = QobuzClient::new()?;
//! client.init().await?;
//! let session = client.login_with_token(user_id, token).await?;
//! let album = client.get_album("0060253780968").await?;
//! ```

pub mod auth;
pub mod client;
pub mod endpoints;
pub mod error;
pub mod forbidden_breaker;
pub mod link_resolver;
pub mod lyrics;
pub mod net_diag;
pub mod offline_gate;
pub mod performers;
pub mod purchases;
pub mod retry;

// Re-export main types
pub use client::QobuzClient;
pub use error::{ApiError, Result};
pub use link_resolver::{resolve_link, LinkResolverError, ResolvedLink};
pub use lyrics::{
    merge_translation_into, QobuzLyricsContent, QobuzLyricsDocument, QobuzLyricsLine,
    QobuzLyricsPlainLine, QobuzLyricsPublisher, QobuzLyricsTranslation, QobuzLyricsUrls,
    QobuzLyricsWord,
};
