//! User-facing answers for features removed for legal reasons (see
//! COMPLIANCE.md at the repository root). The QML still offers some of these
//! entry points; they land here instead of doing anything.

/// "Make offline" / "Download for offline" on Qobuz content. Saving Qobuz
/// streams to disk outside Qobuz's own apps was removed.
pub fn offline_unavailable() {
    crate::toast_qt::error(qbz_i18n::t(
        "Offline downloads of Qobuz tracks aren't available. Local files, purchases you downloaded from qobuz.com and your media servers still play offline.",
    ));
}
