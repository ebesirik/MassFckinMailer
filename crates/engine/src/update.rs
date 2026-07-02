//! Over-the-air updates via GitHub Releases — no external service required.
//!
//! The build's channel is inferred from its own version string (embedded at
//! build time as `MFM_VERSION`): a `-nightly.` / `-beta.` suffix selects the
//! rolling `nightly` / `beta` pre-release; anything else is stable (queried via
//! `/releases/latest`, which GitHub scopes to non-pre-releases). We compare the
//! remote version with semver, and on apply download the matching platform
//! asset, verify its SHA-256 when GitHub provides a digest, then hand off:
//! Windows runs the installer (which closes + upgrades + relaunches us), other
//! platforms swap the running binary in place and relaunch.

use serde::Deserialize;
use sha2::{Digest, Sha256};
use std::path::PathBuf;

const OWNER: &str = "ebesirik";
const REPO: &str = "MassFckinMailer";
const UA: &str = concat!("MassFckinMailer/", env!("CARGO_PKG_VERSION"));

/// Build target as used in CI release-asset names (`matrix.target_name`).
#[cfg(all(target_os = "windows", target_arch = "x86_64"))]
const TARGET: &str = "windows-x86_64";
#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
const TARGET: &str = "linux-x86_64";
#[cfg(all(target_os = "macos", target_arch = "aarch64"))]
const TARGET: &str = "macos-aarch64";
#[cfg(not(any(
    all(target_os = "windows", target_arch = "x86_64"),
    all(target_os = "linux", target_arch = "x86_64"),
    all(target_os = "macos", target_arch = "aarch64"),
)))]
const TARGET: &str = "unsupported";

/// A newer release found for this build's channel + platform.
#[derive(Debug, Clone)]
pub struct UpdateInfo {
    /// Human/semver version, e.g. `0.2.0` or `0.1.0-nightly.20260703.42`.
    pub version: String,
    /// Release notes (marker lines stripped), truncated for display.
    pub notes: String,
    /// The release's web page, for a "view details" link.
    pub html_url: String,
    pub asset_name: String,
    pub download_url: String,
    /// Lowercase hex SHA-256 if GitHub supplied an asset digest.
    pub sha256: Option<String>,
    /// Windows: apply by running the Inno installer. Otherwise: swap the binary.
    pub uses_installer: bool,
}

#[derive(Clone, Copy)]
enum Channel {
    Stable,
    Beta,
    Nightly,
}

fn channel_of(version: &str) -> Channel {
    if version.contains("-nightly") {
        Channel::Nightly
    } else if version.contains("-beta") {
        Channel::Beta
    } else {
        Channel::Stable
    }
}

fn release_url(ch: Channel) -> String {
    let base = format!("https://api.github.com/repos/{OWNER}/{REPO}/releases");
    match ch {
        Channel::Stable => format!("{base}/latest"),
        Channel::Beta => format!("{base}/tags/beta"),
        Channel::Nightly => format!("{base}/tags/nightly"),
    }
}

#[derive(Deserialize)]
struct GhRelease {
    tag_name: String,
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    body: Option<String>,
    #[serde(default)]
    html_url: String,
    #[serde(default)]
    assets: Vec<GhAsset>,
}

#[derive(Deserialize)]
struct GhAsset {
    name: String,
    browser_download_url: String,
    #[serde(default)]
    digest: Option<String>,
}

/// Channel releases embed their exact version in the release body as a
/// `MFM_VERSION=…` marker line (the tag itself is the fixed `beta`/`nightly`).
fn parse_marker(body: &str) -> Option<String> {
    body.lines().find_map(|l| {
        l.trim()
            .strip_prefix("MFM_VERSION=")
            .map(str::trim)
            .filter(|v| !v.is_empty())
            .map(str::to_string)
    })
}

fn strip_marker(body: &str) -> String {
    body.lines()
        .filter(|l| !l.trim().starts_with("MFM_VERSION="))
        .collect::<Vec<_>>()
        .join("\n")
        .trim()
        .chars()
        .take(2000)
        .collect()
}

fn remote_version(rel: &GhRelease, ch: Channel) -> Option<String> {
    match ch {
        Channel::Stable => Some(rel.tag_name.trim_start_matches('v').to_string()),
        Channel::Beta | Channel::Nightly => rel
            .body
            .as_deref()
            .and_then(parse_marker)
            .or_else(|| rel.name.clone()),
    }
}

fn is_newer(current: &str, remote: &str) -> bool {
    match (
        semver::Version::parse(current),
        semver::Version::parse(remote),
    ) {
        (Ok(cur), Ok(rem)) => rem > cur,
        // If either isn't valid semver, treat any difference as an update.
        _ => remote != current,
    }
}

fn select_asset(rel: &GhRelease, uses_installer: bool) -> Option<&GhAsset> {
    rel.assets.iter().find(|a| {
        a.name.contains(TARGET)
            && if uses_installer {
                a.name.ends_with("-setup.exe")
            } else {
                a.name.ends_with(".tar.gz")
            }
    })
}

fn http_client() -> Result<reqwest::Client, String> {
    reqwest::Client::builder()
        .user_agent(UA)
        .connect_timeout(std::time::Duration::from_secs(10))
        .build()
        .map_err(|e| e.to_string())
}

/// Query GitHub for a newer release on this build's channel. `Ok(None)` means
/// up to date; `Err` is a soft failure (offline, rate-limited, …).
pub async fn check(current_version: &str) -> Result<Option<UpdateInfo>, String> {
    if TARGET == "unsupported" {
        return Err("no prebuilt binaries for this platform".into());
    }
    let ch = channel_of(current_version);
    let resp = http_client()?
        .get(release_url(ch))
        .header("Accept", "application/vnd.github+json")
        .send()
        .await
        .map_err(|e| e.to_string())?;
    if !resp.status().is_success() {
        return Err(format!("GitHub returned HTTP {}", resp.status().as_u16()));
    }
    let rel: GhRelease = resp.json().await.map_err(|e| e.to_string())?;

    let Some(remote) = remote_version(&rel, ch) else {
        return Err("could not determine the remote version".into());
    };
    if !is_newer(current_version, &remote) {
        return Ok(None);
    }

    let uses_installer = cfg!(target_os = "windows");
    let (asset_name, download_url, sha256) = {
        let Some(asset) = select_asset(&rel, uses_installer) else {
            return Err(format!("release has no asset for this platform ({TARGET})"));
        };
        let sha256 = asset
            .digest
            .as_deref()
            .and_then(|d| d.strip_prefix("sha256:"))
            .map(|s| s.to_ascii_lowercase());
        (
            asset.name.clone(),
            asset.browser_download_url.clone(),
            sha256,
        )
    };

    Ok(Some(UpdateInfo {
        version: remote,
        notes: rel.body.as_deref().map(strip_marker).unwrap_or_default(),
        html_url: rel.html_url,
        asset_name,
        download_url,
        sha256,
        uses_installer,
    }))
}

/// Download, verify, and apply an update, then hand off. On success the caller
/// should quit — a new/updated process has been started (or the installer is
/// running and will relaunch us).
pub async fn apply(info: &UpdateInfo) -> Result<(), String> {
    let bytes = http_client()?
        .get(&info.download_url)
        .send()
        .await
        .map_err(|e| e.to_string())?
        .error_for_status()
        .map_err(|e| e.to_string())?
        .bytes()
        .await
        .map_err(|e| e.to_string())?;

    if let Some(expected) = &info.sha256 {
        let mut hasher = Sha256::new();
        hasher.update(&bytes);
        let got = hex(&hasher.finalize());
        if !got.eq_ignore_ascii_case(expected) {
            return Err(format!(
                "checksum mismatch (expected {expected}, got {got})"
            ));
        }
    }

    // File I/O, extraction and the self-replace are blocking.
    let info = info.clone();
    tokio::task::spawn_blocking(move || {
        if info.uses_installer {
            apply_installer(&info.asset_name, &bytes)
        } else {
            apply_portable(&bytes)
        }
    })
    .await
    .map_err(|e| e.to_string())?
}

fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write;
    bytes.iter().fold(String::new(), |mut s, b| {
        let _ = write!(s, "{b:02x}");
        s
    })
}

/// Windows: write the installer to a temp file and launch it silently. Inno's
/// `CloseApplications` closes us, upgrades in place, and relaunches.
fn apply_installer(asset_name: &str, bytes: &[u8]) -> Result<(), String> {
    let path = std::env::temp_dir().join(asset_name);
    std::fs::write(&path, bytes).map_err(|e| e.to_string())?;
    let mut cmd = std::process::Command::new(&path);
    cmd.args(["/SILENT", "/SUPPRESSMSGBOXES", "/NORESTART"]);
    cmd.spawn().map_err(|e| e.to_string())?;
    Ok(())
}

/// Non-Windows: extract the binary from the `.tar.gz`, swap it over the running
/// executable, and relaunch.
fn apply_portable(archive: &[u8]) -> Result<(), String> {
    let new_exe = extract_binary(archive)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = std::fs::metadata(&new_exe)
            .map_err(|e| e.to_string())?
            .permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&new_exe, perms).map_err(|e| e.to_string())?;
    }
    self_replace::self_replace(&new_exe).map_err(|e| e.to_string())?;
    let _ = std::fs::remove_file(&new_exe);
    if let Ok(exe) = std::env::current_exe() {
        let _ = std::process::Command::new(exe).spawn();
    }
    Ok(())
}

/// Pull the `massfckinmailer` binary out of the release tarball into a temp file.
fn extract_binary(gz: &[u8]) -> Result<PathBuf, String> {
    use flate2::read::GzDecoder;
    use tar::Archive;

    let out = std::env::temp_dir().join("massfckinmailer-update.bin");
    let mut archive = Archive::new(GzDecoder::new(gz));
    for entry in archive.entries().map_err(|e| e.to_string())? {
        let mut entry = entry.map_err(|e| e.to_string())?;
        let is_bin = entry
            .path()
            .map(|p| p.file_name().and_then(|n| n.to_str()) == Some("massfckinmailer"))
            .unwrap_or(false);
        if is_bin {
            let mut file = std::fs::File::create(&out).map_err(|e| e.to_string())?;
            std::io::copy(&mut entry, &mut file).map_err(|e| e.to_string())?;
            return Ok(out);
        }
    }
    Err("update archive did not contain the app binary".into())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn channel_detection() {
        assert!(matches!(channel_of("0.2.0"), Channel::Stable));
        assert!(matches!(channel_of("0.1.0-beta.5"), Channel::Beta));
        assert!(matches!(
            channel_of("0.1.0-nightly.20260703.42"),
            Channel::Nightly
        ));
    }

    #[test]
    fn semver_ordering_across_builds() {
        assert!(is_newer("0.1.0", "0.2.0"));
        assert!(!is_newer("0.2.0", "0.1.0"));
        assert!(!is_newer("0.2.0", "0.2.0"));
        // Nightly build numbers increase; date then run number both work.
        assert!(is_newer(
            "0.1.0-nightly.20260703.42",
            "0.1.0-nightly.20260704.7"
        ));
        assert!(is_newer("0.1.0-beta.5", "0.1.0-beta.6"));
    }

    #[test]
    fn marker_roundtrip() {
        let body = "MFM_VERSION=0.1.0-nightly.20260703.42\n\nSome notes here.";
        assert_eq!(
            parse_marker(body).as_deref(),
            Some("0.1.0-nightly.20260703.42")
        );
        assert_eq!(strip_marker(body), "Some notes here.");
    }
}
