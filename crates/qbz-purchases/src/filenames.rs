//! Filesystem-safe names (copied from upstream QBZ `qbz-offline-cache::metadata`).

/// Sanitize filename to be ASCII-safe and filesystem-compatible
pub fn sanitize_filename(name: &str) -> String {
    // Remove or replace invalid characters
    let invalid_chars = ['/', '\\', ':', '*', '?', '"', '<', '>', '|'];

    let mut sanitized = name
        .chars()
        .map(|c| {
            if invalid_chars.contains(&c) {
                '-'
            } else if c.is_ascii() || c.is_alphanumeric() {
                c
            } else {
                '-'
            }
        })
        .collect::<String>();

    // Replace multiple consecutive dashes with single dash
    while sanitized.contains("--") {
        sanitized = sanitized.replace("--", "-");
    }

    // Trim dashes and whitespace from ends
    sanitized = sanitized.trim_matches('-').trim().to_string();

    // Limit length to 200 BYTES (leaving room for extension and path).
    //
    // The cap is deliberately in bytes, not chars: it is what the shipped
    // implementation used, and these strings become on-disk path segments that
    // the library-to-registry join matches by EXACT string equality
    // (`database.rs`, the `qobuz_purchase` source stamp). Recomputing the cap in
    // chars would silently relocate every already-downloaded non-ASCII album.
    //
    // `String::truncate` panics when the index is not a char boundary, and that
    // is reachable here rather than theoretical: the mapping above keeps every
    // Unicode ALPHANUMERIC (`c.is_alphanumeric()` is Unicode-aware), so accented
    // Latin, Greek, Cyrillic and CJK titles all survive as multibyte characters.
    // A 200-byte cut lands mid-character for any such title long enough to hit
    // it. Walk back to the nearest boundary instead.
    if sanitized.len() > 200 {
        let end = (0..=200)
            .rev()
            .find(|&i| sanitized.is_char_boundary(i))
            .unwrap_or(0);
        sanitized.truncate(end);
        sanitized = sanitized.trim_matches('-').trim().to_string();
    }

    // If empty after sanitization, use fallback
    if sanitized.is_empty() {
        sanitized = "track".to_string();
    }

    sanitized
}
