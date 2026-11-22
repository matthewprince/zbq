//! A Firefox for the Qobuz engine (`firefox_engine.rs`) when none is installed: the
//! current Firefox ESR for Linux x86-64, downloaded from Mozilla, checked against the
//! SHA-512 Mozilla publishes for that release, and unpacked into ZBQ's data folder.
//! Nothing is installed system-wide; a Firefox the user installs later is preferred.

use std::path::PathBuf;

use sha2::{Digest, Sha512};
use tokio::io::AsyncWriteExt;

const ESR_URL: &str = "https://download.mozilla.org/?product=firefox-esr-latest-ssl&os=linux64&lang=en-US";

/// Where a downloaded Firefox lives (its binary, whether present or not).
pub fn local_binary() -> Option<PathBuf> {
    Some(dirs::data_dir()?.join("zbq").join("firefox").join("firefox"))
}

/// Download, verify and unpack Firefox ESR; returns its binary.
pub async fn provision() -> Result<PathBuf, String> {
    let root = dirs::data_dir().ok_or("no data directory")?.join("zbq");
    let bin = root.join("firefox").join("firefox");
    if bin.is_file() {
        return Ok(bin);
    }
    std::fs::create_dir_all(&root).map_err(|e| e.to_string())?;
    log::info!("[firefox-engine] no Firefox installed: downloading Firefox ESR from Mozilla");
    crate::toast_qt::info(qbz_i18n::t(
        "Setting up bit-perfect Qobuz playback: downloading Firefox ESR from Mozilla (about 80 MB).",
    ));
    let client = reqwest::Client::builder()
        .user_agent("zbq")
        .build()
        .map_err(|e| e.to_string())?;
    let mut response = client
        .get(ESR_URL)
        .send()
        .await
        .and_then(|r| r.error_for_status())
        .map_err(|e| format!("Firefox download: {e}"))?;
    // .../pub/firefox/releases/<version>/linux-x86_64/en-US/<file>
    let url = response.url().clone();
    let parts: Vec<&str> = url.path().split('/').collect();
    let version = parts
        .iter()
        .position(|p| *p == "releases")
        .and_then(|i| parts.get(i + 1))
        .ok_or("unexpected Firefox download address")?
        .to_string();
    let file = parts.last().copied().unwrap_or_default().to_string();
    let sums_url = format!(
        "{}://{}/pub/firefox/releases/{version}/SHA512SUMS",
        url.scheme(),
        url.host_str().unwrap_or("archive.mozilla.org")
    );
    let sums = client
        .get(&sums_url)
        .send()
        .await
        .and_then(|r| r.error_for_status())
        .map_err(|e| format!("Firefox checksums: {e}"))?
        .text()
        .await
        .map_err(|e| e.to_string())?;
    let wanted = format!("linux-x86_64/en-US/{file}");
    let expected = sums
        .lines()
        .find_map(|line| {
            let (hash, name) = line.split_once(char::is_whitespace)?;
            (name.trim() == wanted).then(|| hash.to_ascii_lowercase())
        })
        .ok_or("Mozilla lists no checksum for this Firefox download")?;

    let part = root.join("firefox-download.part");
    let mut out = tokio::fs::File::create(&part).await.map_err(|e| e.to_string())?;
    let mut hasher = Sha512::new();
    while let Some(chunk) = response.chunk().await.map_err(|e| format!("Firefox download: {e}"))? {
        hasher.update(&chunk);
        out.write_all(&chunk).await.map_err(|e| e.to_string())?;
    }
    out.flush().await.map_err(|e| e.to_string())?;
    drop(out);
    let got: String = hasher.finalize().iter().map(|b| format!("{b:02x}")).collect();
    if got != expected {
        let _ = std::fs::remove_file(&part);
        return Err("the Firefox download did not match Mozilla's checksum".into());
    }
    let staging = root.join("firefox-staging");
    let _ = std::fs::remove_dir_all(&staging);
    std::fs::create_dir_all(&staging).map_err(|e| e.to_string())?;
    let status = tokio::process::Command::new("tar")
        .arg("-xf")
        .arg(&part)
        .arg("-C")
        .arg(&staging)
        .status()
        .await
        .map_err(|e| format!("unpacking Firefox: {e}"))?;
    let _ = std::fs::remove_file(&part);
    if !status.success() || !staging.join("firefox").join("firefox").is_file() {
        let _ = std::fs::remove_dir_all(&staging);
        return Err("unpacking Firefox failed".into());
    }
    let _ = std::fs::remove_dir_all(root.join("firefox"));
    std::fs::rename(staging.join("firefox"), root.join("firefox")).map_err(|e| e.to_string())?;
    let _ = std::fs::remove_dir_all(&staging);
    log::info!("[firefox-engine] Firefox {version} ready at {}", bin.display());
    crate::toast_qt::info(qbz_i18n::t("Firefox is ready: Qobuz plays bit-perfect."));
    Ok(bin)
}
