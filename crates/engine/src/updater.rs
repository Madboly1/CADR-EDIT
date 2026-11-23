//! User-controlled in-app GitHub Release updater for CADR Studio (`Madboly1/CADR-EDIT`).
//!
//! - Queries `https://api.github.com/repos/Madboly1/CADR-EDIT/releases/latest` (pure-Rust HTTPS
//!   via `ureq` + `rustls-rustcrypto` + OS platform certificate verifier; cleanly stubbed on
//!   `wasm32`).
//! - Compares remote release tags (e.g. `v0.21.0`) against `env!("CARGO_PKG_VERSION")`.
//! - Streams the Windows executable asset (`CADR-Studio-windows-x86_64.exe` or `.zip`) into a
//!   temporary staging directory with live progress tracking and header validation.
//! - On Windows, spawns a detached helper script that waits for the current PID to exit, replaces
//!   the executable in place, cleans up temporary files, and relaunches the updated binary.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// GitHub repository hosting CADR Studio releases.
pub const GITHUB_REPO: &str = "Madboly1/CADR-EDIT";
/// GitHub Releases API endpoint for the latest published release.
pub const LATEST_RELEASE_API: &str = "https://api.github.com/repos/Madboly1/CADR-EDIT/releases/latest";
/// Preferred standalone Windows x86_64 executable asset name.
pub const WINDOWS_EXE_ASSET: &str = "CADR-Studio-windows-x86_64.exe";
/// Compiled application crate version.
pub const CURRENT_VERSION: &str = env!("CARGO_PKG_VERSION");

/// Upper bound on the GitHub Releases API JSON payload (4 MiB).
#[cfg(not(target_arch = "wasm32"))]
const MAX_API_JSON_BYTES: u64 = 4 * 1024 * 1024;
/// Upper bound on a downloaded release asset (1 GiB).
#[cfg(not(target_arch = "wasm32"))]
const MAX_DOWNLOAD_BYTES: u64 = 1024 * 1024 * 1024;
/// Minimum plausible size for a real compiled Windows binary (64 KiB).
#[cfg(not(target_arch = "wasm32"))]
const MIN_VALID_BINARY_BYTES: u64 = 64 * 1024;

/// Parsed semantic version (`major.minor.patch` with optional pre-release suffix).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SemVer {
    pub major: u64,
    pub minor: u64,
    pub patch: u64,
    pub pre: Option<String>,
}

impl Ord for SemVer {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        (self.major, self.minor, self.patch).cmp(&(other.major, other.minor, other.patch)).then_with(|| match (&self.pre, &other.pre) {
            (None, None) => std::cmp::Ordering::Equal,
            // Stable release outranks any pre-release of the same major.minor.patch.
            (None, Some(_)) => std::cmp::Ordering::Greater,
            (Some(_), None) => std::cmp::Ordering::Less,
            (Some(a), Some(b)) => a.cmp(b),
        })
    }
}

impl PartialOrd for SemVer {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

/// Parse a version tag such as `"v0.21.0"`, `"0.20.1"`, or `"v0.21.0-rc.1"`.
pub fn parse_semver(raw: &str) -> Option<SemVer> {
    let trimmed = raw.trim().trim_start_matches(['v', 'V']);
    if trimmed.is_empty() {
        return None;
    }
    // Drop optional build metadata (`+...`) before splitting pre-release (`-...`).
    let no_build = trimmed.split('+').next()?;
    let (core, pre) = match no_build.split_once('-') {
        Some((c, p)) if !p.is_empty() => (c, Some(p.to_string())),
        Some((c, _)) => (c, None),
        None => (no_build, None),
    };
    let mut parts = core.split('.');
    let major = parts.next()?.parse::<u64>().ok()?;
    let minor = parts.next()?.parse::<u64>().ok()?;
    let patch = match parts.next() {
        Some(p) => p.parse::<u64>().ok()?,
        None => 0,
    };
    if parts.next().is_some() {
        return None;
    }
    Some(SemVer { major, minor, patch, pre })
}

/// True when `remote` parses as a strictly newer semantic version than `current`.
pub fn is_newer_version(current: &str, remote: &str) -> bool {
    match (parse_semver(current), parse_semver(remote)) {
        (Some(cur), Some(rem)) => rem > cur,
        _ => false,
    }
}

/// Metadata for an available GitHub release.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ReleaseInfo {
    pub tag_name: String,
    pub version: String,
    pub name: String,
    pub release_notes: String,
    pub html_url: String,
    pub asset_name: Option<String>,
    pub download_url: Option<String>,
    pub asset_size: u64,
    pub is_zip: bool,
    #[serde(default)]
    pub msi_asset_name: Option<String>,
    #[serde(default)]
    pub msi_download_url: Option<String>,
    #[serde(default)]
    pub msi_asset_size: u64,
    #[serde(default)]
    pub msi_sha256: Option<String>,
}

/// Outcome of checking the latest GitHub release against the running binary version.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum UpdateCheckResult {
    UpToDate { current: String, latest: String },
    UpdateAvailable(ReleaseInfo),
}

struct AssetCandidate {
    name: String,
    url: String,
    size: u64,
    is_zip: bool,
    priority: u8,
}

/// Select the best Windows x86_64 release asset from a GitHub release's `assets` JSON array.
fn pick_windows_asset(assets: &[Value]) -> Option<AssetCandidate> {
    let mut best: Option<AssetCandidate> = None;
    for asset in assets {
        let Some(name) = asset.get("name").and_then(Value::as_str) else { continue };
        let Some(url) = asset.get("browser_download_url").and_then(Value::as_str) else { continue };
        if !url.starts_with("https://") {
            continue;
        }
        let low = name.to_ascii_lowercase();
        let is_win_x64 = low.contains("win") && (low.contains("x86_64") || low.contains("x64"));
        let (priority, is_zip) = if name.eq_ignore_ascii_case(WINDOWS_EXE_ASSET) {
            (100, false)
        } else if low.ends_with(".exe") && is_win_x64 {
            (90, false)
        } else if low.ends_with(".exe") && !low.contains("cli") {
            (80, false)
        } else if low.ends_with(".zip") && is_win_x64 {
            (70, true)
        } else if low.ends_with(".zip") && low.contains("win") {
            (60, true)
        } else {
            continue;
        };
        let size = asset.get("size").and_then(Value::as_u64).unwrap_or(0);
        if best.as_ref().is_none_or(|b| priority > b.priority) {
            best = Some(AssetCandidate { name: name.to_string(), url: url.to_string(), size, is_zip, priority });
        }
    }
    best
}

/// Candidate Windows MSI installer asset from GitHub Releases.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MsiAssetCandidate {
    pub name: String,
    pub url: String,
    pub size: u64,
    /// Lowercase 64-digit SHA-256 from the asset's `digest` field; `None` when missing or malformed.
    pub sha256: Option<String>,
}

/// Normalize a bare SHA-256 hex string to 64 lowercase digits; `None` when malformed.
pub fn normalize_sha256_hex(raw: &str) -> Option<String> {
    let hex = raw.trim();
    if hex.len() != 64 || !hex.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    Some(hex.to_ascii_lowercase())
}

/// Parse a GitHub asset `digest` value of the form `sha256:<64 hex digits>`.
pub fn parse_sha256_digest(raw: &str) -> Option<String> {
    let hex = raw.trim().strip_prefix("sha256:")?;
    normalize_sha256_hex(hex)
}

/// Select the Windows x86_64 / x64 MSI release asset from a GitHub release's `assets` JSON array.
///
/// Only assets whose download URL is on [`release_download_prefix`] are considered.
pub fn pick_windows_msi_asset(assets: &[Value]) -> Option<MsiAssetCandidate> {
    for asset in assets {
        let Some(name) = asset.get("name").and_then(Value::as_str) else { continue };
        let Some(url) = asset.get("browser_download_url").and_then(Value::as_str) else { continue };
        if !is_trusted_release_url(url) {
            continue;
        }
        let low = name.to_ascii_lowercase();
        let is_win_x64 = low.contains("win") && (low.contains("x86_64") || low.contains("x64"));
        if low.ends_with(".msi") && is_win_x64 {
            let size = asset.get("size").and_then(Value::as_u64).unwrap_or(0);
            let sha256 = asset.get("digest").and_then(Value::as_str).and_then(parse_sha256_digest);
            return Some(MsiAssetCandidate { name: name.to_string(), url: url.to_string(), size, sha256 });
        }
    }
    None
}

/// Parse a GitHub `/releases/latest` JSON response and compare it to `current_version`.
pub fn parse_github_release_json(bytes: &[u8], current_version: &str) -> Result<UpdateCheckResult, String> {
    let root: Value = serde_json::from_slice(bytes).map_err(|e| format!("invalid GitHub release JSON: {e}"))?;
    let tag_name =
        root.get("tag_name").and_then(Value::as_str).filter(|s| !s.trim().is_empty()).ok_or_else(|| "release JSON missing `tag_name`".to_string())?;
    let version = tag_name.trim().trim_start_matches(['v', 'V']).to_string();

    if !is_newer_version(current_version, tag_name) {
        return Ok(UpdateCheckResult::UpToDate { current: current_version.to_string(), latest: version });
    }

    let name = root.get("name").and_then(Value::as_str).filter(|s| !s.trim().is_empty()).unwrap_or(tag_name).to_string();
    let release_notes = root.get("body").and_then(Value::as_str).unwrap_or("").trim().to_string();
    let html_url = root
        .get("html_url")
        .and_then(Value::as_str)
        .filter(|u| u.starts_with("https://"))
        .unwrap_or("https://github.com/Madboly1/CADR-EDIT/releases/latest")
        .to_string();

    let asset = root.get("assets").and_then(Value::as_array).and_then(|arr| pick_windows_asset(arr));
    let (asset_name, download_url, asset_size, is_zip) = match asset {
        Some(a) => (Some(a.name), Some(a.url), a.size, a.is_zip),
        None => (None, None, 0, false),
    };

    let msi_asset = root.get("assets").and_then(Value::as_array).and_then(|arr| pick_windows_msi_asset(arr));
    let (msi_asset_name, msi_download_url, msi_asset_size, msi_sha256) = match msi_asset {
        Some(a) => (Some(a.name), Some(a.url), a.size, a.sha256),
        None => (None, None, 0, None),
    };

    Ok(UpdateCheckResult::UpdateAvailable(ReleaseInfo {
        tag_name: tag_name.to_string(),
        version,
        name,
        release_notes,
        html_url,
        asset_name,
        download_url,
        asset_size,
        is_zip,
        msi_asset_name,
        msi_download_url,
        msi_asset_size,
        msi_sha256,
    }))
}

/// Pure-Rust HTTPS client configured with `rustls-rustcrypto` and OS platform root certificates.
#[cfg(not(target_arch = "wasm32"))]
fn http_agent() -> ureq::Agent {
    let provider = std::sync::Arc::new(rustls_rustcrypto::provider());
    ureq::Agent::config_builder()
        .tls_config(
            ureq::tls::TlsConfig::builder()
                .provider(ureq::tls::TlsProvider::Rustls)
                .root_certs(ureq::tls::RootCerts::PlatformVerifier)
                .unversioned_rustls_crypto_provider(provider)
                .build(),
        )
        .timeout_connect(Some(std::time::Duration::from_secs(15)))
        .build()
        .into()
}

/// Query GitHub Releases (`Madboly1/CADR-EDIT`) for the latest release and compare against
/// `current_version`.
#[cfg(not(target_arch = "wasm32"))]
pub fn check_latest_release(current_version: &str) -> Result<UpdateCheckResult, String> {
    use std::io::Read;
    let agent = http_agent();
    let ua = format!("CADR-Studio/{current_version}");
    let resp = agent
        .get(LATEST_RELEASE_API)
        .header("User-Agent", &ua)
        .header("Accept", "application/vnd.github+json")
        .call()
        .map_err(|e| format!("failed to contact GitHub Releases: {e}"))?;
    let mut body = resp.into_body();
    let mut raw = Vec::new();
    body.as_reader().take(MAX_API_JSON_BYTES + 1).read_to_end(&mut raw).map_err(|e| format!("failed reading GitHub release response: {e}"))?;
    if raw.len() as u64 > MAX_API_JSON_BYTES {
        return Err("GitHub release response exceeded size limit".into());
    }
    parse_github_release_json(&raw, current_version)
}

/// Stub for `wasm32`: browsers cannot query or self-replace native binaries.
#[cfg(target_arch = "wasm32")]
pub fn check_latest_release(_current_version: &str) -> Result<UpdateCheckResult, String> {
    Err("Update checks are not supported in the web browser.".into())
}

/// OLE compound document magic header found at the start of all `.msi` files (CFBF).
pub const MSI_COMPOUND_MAGIC: [u8; 8] = [0xD0, 0xCF, 0x11, 0xE0, 0xA1, 0xB1, 0x1A, 0xE1];

/// Validate staged MSI bytes: first 8 bytes must match the OLE compound document magic.
pub fn validate_msi_bytes(bytes: &[u8]) -> Result<(), String> {
    if bytes.len() < MSI_COMPOUND_MAGIC.len() {
        return Err(format!("downloaded MSI is too short ({} bytes)", bytes.len()));
    }
    if !bytes.starts_with(&MSI_COMPOUND_MAGIC) {
        return Err("downloaded file is not a valid MSI installer (missing OLE compound document magic)".into());
    }
    Ok(())
}

/// Prefix every trusted release asset URL must start with, built from [`GITHUB_REPO`].
pub fn release_download_prefix() -> String {
    format!("https://github.com/{GITHUB_REPO}/releases/download/")
}

/// Whether `url` is on the trusted CADR Studio GitHub release download prefix.
pub fn is_trusted_release_url(url: &str) -> bool {
    url.starts_with(&release_download_prefix())
}

/// Check that a release can be installed through the MSI path and return its URL and normalized
/// SHA-256 digest. Fails closed when the URL is not on the trusted prefix or the digest is missing
/// or malformed.
pub fn msi_download_requirements(release: &ReleaseInfo) -> Result<(&str, String), String> {
    let url = release.msi_download_url.as_deref().ok_or_else(|| "release has no downloadable MSI asset".to_string())?;
    if !is_trusted_release_url(url) {
        return Err("MSI download URL is not on the trusted release location; refusing to download".into());
    }
    let published = release.msi_sha256.as_deref().and_then(normalize_sha256_hex);
    let sha256 = published.ok_or_else(|| "release does not publish a valid SHA-256 digest for the MSI; refusing to install".to_string())?;
    Ok((url, sha256))
}

/// SHA-256 of `bytes` as 64 lowercase hex characters.
pub fn sha256_bytes_hex(bytes: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    format!("{:x}", hasher.finalize())
}

/// SHA-256 of the file at `path` (streamed) as 64 lowercase hex characters.
pub fn sha256_file_hex(path: &std::path::Path) -> Result<String, String> {
    use sha2::{Digest, Sha256};
    use std::io::Read;
    let mut file = std::fs::File::open(path).map_err(|e| format!("failed opening file for hashing: {e}"))?;
    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; 64 * 1024];
    loop {
        let n = file.read(&mut buf).map_err(|e| format!("failed reading file for hashing: {e}"))?;
        if n == 0 {
            break;
        }
        if let Some(chunk) = buf.get(..n) {
            hasher.update(chunk);
        }
    }
    Ok(format!("{:x}", hasher.finalize()))
}

/// Compare the SHA-256 of `path` with `expected_hex`. On any failure or mismatch the file is
/// deleted and an error is returned.
pub fn verify_file_sha256(path: &std::path::Path, expected_hex: &str) -> Result<(), String> {
    let Some(expected) = normalize_sha256_hex(expected_hex) else {
        let _ = std::fs::remove_file(path);
        return Err("expected SHA-256 digest is malformed".into());
    };
    match sha256_file_hex(path) {
        Ok(actual) if actual == expected => Ok(()),
        Ok(_) => {
            let _ = std::fs::remove_file(path);
            Err("downloaded installer does not match the published SHA-256 digest; it was deleted".into())
        }
        Err(e) => {
            let _ = std::fs::remove_file(path);
            Err(e)
        }
    }
}

/// In-memory record of staged MSI files and the digest they were verified against. Kept in memory
/// only (never in a file an attacker could also rewrite) so the launch step can re-hash the file.
fn verified_msi_registry() -> &'static std::sync::Mutex<Vec<(PathBuf, String)>> {
    static REGISTRY: std::sync::OnceLock<std::sync::Mutex<Vec<(PathBuf, String)>>> = std::sync::OnceLock::new();
    REGISTRY.get_or_init(|| std::sync::Mutex::new(Vec::new()))
}

/// Remember that `path` was verified against `digest_hex`.
pub fn record_verified_msi(path: &std::path::Path, digest_hex: &str) {
    let mut reg = verified_msi_registry().lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    reg.retain(|(p, _)| p != path);
    reg.push((path.to_path_buf(), digest_hex.to_string()));
}

/// Digest recorded for `path` by [`record_verified_msi`], if any.
pub fn recorded_msi_digest(path: &std::path::Path) -> Option<String> {
    let reg = verified_msi_registry().lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    reg.iter().find(|(p, _)| p == path).map(|(_, d)| d.clone())
}

/// Re-verify a staged MSI immediately before launch. Fails closed (and deletes the file on a
/// mismatch) when the file was not verified by this process or its hash changed.
pub fn verify_staged_msi_before_launch(path: &std::path::Path) -> Result<(), String> {
    let Some(expected) = recorded_msi_digest(path) else {
        let _ = std::fs::remove_file(path);
        return Err("staged installer was not verified by this session; refusing to run it".into());
    };
    verify_file_sha256(path, &expected)
}

/// Validate a staged Windows executable (PE `"MZ"` header + non-empty minimum size).
pub fn validate_windows_exe_bytes(bytes: &[u8], min_size: usize) -> Result<(), String> {
    if bytes.len() < min_size {
        return Err(format!("downloaded binary is too small ({} bytes)", bytes.len()));
    }
    if !bytes.starts_with(b"MZ") {
        return Err("downloaded file is not a valid Windows executable (missing PE header)".into());
    }
    Ok(())
}

/// Extract the CADR Studio Windows executable from a downloaded release `.zip` archive.
pub fn extract_exe_from_zip(zip_bytes: &[u8]) -> Result<Vec<u8>, String> {
    let entries = filmcraft_media::mogrt::list_zip_entries(zip_bytes).map_err(|e| format!("invalid release ZIP: {e}"))?;
    let pick = entries
        .iter()
        .find(|e| {
            let low = e.name.to_ascii_lowercase();
            low.ends_with(".exe") && !low.contains("cli") && (low.contains("cadr") || low.contains("filmcraft"))
        })
        .or_else(|| entries.iter().find(|e| e.name.to_ascii_lowercase().ends_with(".exe") && !e.name.to_ascii_lowercase().contains("cli")))
        .ok_or_else(|| "release ZIP does not contain a Windows executable".to_string())?;
    let limit = 256 * 1024 * 1024;
    let exe_bytes = filmcraft_media::mogrt::extract_zip_entry(zip_bytes, pick, limit).map_err(|e| format!("failed extracting {}: {e}", pick.name))?;
    validate_windows_exe_bytes(&exe_bytes, 2)?;
    Ok(exe_bytes)
}

/// Download the release asset for `release` into the OS temp directory with progress updates.
///
/// `on_progress(downloaded, total)` returns `false` to cancel the download. Returns the verified
/// path to the staged `.exe` or `.msi` ready for self-replacement or MSI upgrade.
#[cfg(not(target_arch = "wasm32"))]
pub fn download_release_asset(release: &ReleaseInfo, on_progress: &mut dyn FnMut(u64, u64) -> bool) -> Result<PathBuf, String> {
    use std::io::{Read, Write};

    // MSI-managed installs can only be updated through the MSI (the exe swap cannot write there), so
    // there is no fallback: fail closed before any network access when the MSI asset is unusable
    // (untrusted URL, or no valid published SHA-256 digest).
    let is_msi = is_current_install_msi_managed();
    let (url, msi_expected_sha256) = if is_msi {
        let (msi_url, sha256) = msi_download_requirements(release)?;
        (msi_url, Some(sha256))
    } else {
        let exe_url = release.download_url.as_deref().filter(|u| u.starts_with("https://")).ok_or_else(|| "release has no downloadable Windows asset".to_string())?;
        (exe_url, None)
    };
    let stage_dir = std::env::temp_dir().join("cadr-studio-updates");
    std::fs::create_dir_all(&stage_dir).map_err(|e| format!("failed creating update staging folder: {e}"))?;

    let safe_ver: String = release.version.chars().map(|c| if c.is_ascii_alphanumeric() || c == '.' || c == '-' { c } else { '_' }).take(32).collect();
    let ext = match (is_msi, release.is_zip) {
        (true, _) => "msi",
        (false, true) => "zip",
        (false, false) => "exe",
    };
    let pid = std::process::id();
    let part_path = stage_dir.join(format!("cadr-update-{safe_ver}-{pid}.{ext}.part"));
    let final_dl = stage_dir.join(format!("cadr-update-{safe_ver}-{pid}.{ext}"));

    let agent = http_agent();
    let ua = format!("CADR-Studio/{CURRENT_VERSION}");
    let resp = agent.get(url).header("User-Agent", &ua).call().map_err(|e| format!("download failed: {e}"))?;
    let total = if is_msi { release.msi_asset_size } else { release.asset_size };
    let mut body = resp.into_body();
    let mut reader = body.as_reader();
    let file = std::fs::File::create(&part_path).map_err(|e| format!("failed creating temp file: {e}"))?;
    let mut out = std::io::BufWriter::new(file);
    let mut buf = vec![0u8; 64 * 1024];
    let mut got: u64 = 0;

    loop {
        let n = reader.read(&mut buf).map_err(|e| {
            let _ = std::fs::remove_file(&part_path);
            format!("download stream error: {e}")
        })?;
        if n == 0 {
            break;
        }
        got = got.saturating_add(n as u64);
        if got > MAX_DOWNLOAD_BYTES {
            drop(out);
            let _ = std::fs::remove_file(&part_path);
            return Err("downloaded update exceeded 1 GiB safety limit".into());
        }
        if let Some(chunk) = buf.get(..n) {
            out.write_all(chunk).map_err(|e| {
                let _ = std::fs::remove_file(&part_path);
                format!("failed writing temp file: {e}")
            })?;
        }
        if !on_progress(got, total.max(got)) {
            drop(out);
            let _ = std::fs::remove_file(&part_path);
            return Err("Download cancelled.".into());
        }
    }
    out.flush().map_err(|e| format!("failed flushing temp file: {e}"))?;
    drop(out);

    if is_msi {
        if total > 0 && got != total {
            let _ = std::fs::remove_file(&part_path);
            return Err(format!("downloaded MSI size ({got} bytes) does not match expected size ({total} bytes)"));
        }
        if got < MIN_VALID_BINARY_BYTES {
            let _ = std::fs::remove_file(&part_path);
            return Err(format!("downloaded update is incomplete or invalid ({got} bytes)"));
        }
        let _ = std::fs::remove_file(&final_dl);
        std::fs::rename(&part_path, &final_dl).map_err(|e| format!("failed finalizing staged download: {e}"))?;

        let mut head = [0u8; 8];
        let mut check_f = std::fs::File::open(&final_dl).map_err(|e| format!("failed verifying staged MSI: {e}"))?;
        check_f.read_exact(&mut head).map_err(|e| format!("failed reading staged MSI header: {e}"))?;
        // Size and OLE magic above are format checks only; identity is the SHA-256 digest.
        if let Err(e) = validate_msi_bytes(&head) {
            let _ = std::fs::remove_file(&final_dl);
            return Err(e);
        }
        let Some(expected) = msi_expected_sha256.as_deref() else {
            let _ = std::fs::remove_file(&final_dl);
            return Err("MSI digest missing; refusing to stage the installer".into());
        };
        verify_file_sha256(&final_dl, expected)?;
        record_verified_msi(&final_dl, expected);
        return Ok(final_dl);
    }

    if got < MIN_VALID_BINARY_BYTES {
        let _ = std::fs::remove_file(&part_path);
        return Err(format!("downloaded update is incomplete or invalid ({got} bytes)"));
    }
    let _ = std::fs::remove_file(&final_dl);
    std::fs::rename(&part_path, &final_dl).map_err(|e| format!("failed finalizing staged download: {e}"))?;

    if release.is_zip {
        let zip_bytes = std::fs::read(&final_dl).map_err(|e| format!("failed reading staged zip: {e}"))?;
        let exe_bytes = extract_exe_from_zip(&zip_bytes)?;
        let exe_path = stage_dir.join(format!("cadr-update-{safe_ver}-{pid}.exe"));
        std::fs::write(&exe_path, &exe_bytes).map_err(|e| format!("failed writing extracted executable: {e}"))?;
        let _ = std::fs::remove_file(&final_dl);
        return Ok(exe_path);
    }

    // Verify PE header on standalone `.exe`.
    let mut head = [0u8; 2];
    let mut check_f = std::fs::File::open(&final_dl).map_err(|e| format!("failed verifying staged executable: {e}"))?;
    check_f.read_exact(&mut head).map_err(|e| format!("failed reading staged executable header: {e}"))?;
    if &head != b"MZ" {
        let _ = std::fs::remove_file(&final_dl);
        return Err("downloaded file is not a valid Windows executable (missing PE header)".into());
    }
    Ok(final_dl)
}

#[cfg(target_arch = "wasm32")]
pub fn download_release_asset(_release: &ReleaseInfo, _on_progress: &mut dyn FnMut(u64, u64) -> bool) -> Result<PathBuf, String> {
    Err("Downloading updates is not supported in the web browser.".into())
}

fn ps_single_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "''"))
}

/// Build the detached Windows PowerShell helper script that waits for `pid` to exit, replaces
/// `target_exe` with `staged_exe`, removes temporary files, and relaunches `target_exe`.
pub fn build_windows_update_script(pid: u32, staged_exe: &str, target_exe: &str, script_path: &str) -> String {
    let staged_q = ps_single_quote(staged_exe);
    let target_q = ps_single_quote(target_exe);
    let script_q = ps_single_quote(script_path);
    format!(
        "$ErrorActionPreference = 'Stop'\r\n\
         $pidToWait = {pid}\r\n\
         $staged = {staged_q}\r\n\
         $target = {target_q}\r\n\
         $backup = \"$target.old\"\r\n\
         $self = {script_q}\r\n\
         $status = $self + '{STATUS_SUFFIX}'\r\n\
         function Write-Status($text) {{ try {{ Set-Content -LiteralPath $status -Value $text -Encoding UTF8 }} catch {{}} }}\r\n\
         Write-Status 'STARTED'\r\n\
         try {{ Wait-Process -Id $pidToWait -Timeout 30 -ErrorAction SilentlyContinue }} catch {{}}\r\n\
         $stillRunning = [bool](Get-Process -Id $pidToWait -ErrorAction SilentlyContinue)\r\n\
         $replaced = $false\r\n\
         $lastErr = ''\r\n\
         for ($i = 0; $i -lt 40; $i++) {{\r\n\
             try {{\r\n\
                 if (Test-Path -LiteralPath $backup) {{ Remove-Item -LiteralPath $backup -Force -ErrorAction SilentlyContinue }}\r\n\
                 if (Test-Path -LiteralPath $target) {{ Move-Item -LiteralPath $target -Destination $backup -Force }}\r\n\
                 Move-Item -LiteralPath $staged -Destination $target -Force\r\n\
                 $replaced = $true\r\n\
                 break\r\n\
             }} catch {{\r\n\
                 $ex = $_.Exception\r\n\
                 $lastErr = if ($ex -is [System.UnauthorizedAccessException]) {{ 'access denied' }} elseif ($ex -is [System.IO.IOException]) {{ 'file in use or I/O error' }} elseif ($ex -is [System.Management.Automation.ItemNotFoundException]) {{ 'file not found' }} else {{ 'error ' + $ex.GetType().Name }}\r\n\
                 Start-Sleep -Milliseconds 250\r\n\
             }}\r\n\
         }}\r\n\
         if ($replaced) {{\r\n\
             Remove-Item -LiteralPath $backup -Force -ErrorAction SilentlyContinue\r\n\
             Write-Status 'OK'\r\n\
             Start-Process -FilePath $target\r\n\
         }} else {{\r\n\
             if ((Test-Path -LiteralPath $backup) -and -not (Test-Path -LiteralPath $target)) {{ Move-Item -LiteralPath $backup -Destination $target -Force -ErrorAction SilentlyContinue }}\r\n\
             Write-Status ('FAILED: could not replace the program file (previous version restored). ' + $(if ($stillRunning) {{ 'The old process was still running. ' }} else {{ '' }}) + $lastErr)\r\n\
             Remove-Item -LiteralPath $staged -Force -ErrorAction SilentlyContinue\r\n\
             Remove-Item -LiteralPath $self -Force -ErrorAction SilentlyContinue\r\n\
             exit 1\r\n\
         }}\r\n\
         Remove-Item -LiteralPath $staged -Force -ErrorAction SilentlyContinue\r\n\
         Remove-Item -LiteralPath $self -Force -ErrorAction SilentlyContinue\r\n"
    )
}

/// Suffix of the status file each helper script writes next to itself
/// (`apply-update-<pid>.ps1` -> `apply-update-<pid>.ps1.status.txt`), so attempts never collide.
/// Contents: `STARTED`, `OK`, or `FAILED: <bounded reason, no paths>`.
pub const STATUS_SUFFIX: &str = ".status.txt";

/// A `STARTED` file younger than this may belong to a helper that is still running; leave it alone.
const STARTED_STALE_SECS: u64 = 600;

/// Read and clear finished helper status files. Returns a failure text when a previous
/// self-replace attempt reported `FAILED` or was interrupted (stale `STARTED`); `None` when there
/// was no attempt, it succeeded, or a helper may still be running. Unreadable files are ignored.
#[cfg(not(target_arch = "wasm32"))]
pub fn take_last_update_failure() -> Option<String> {
    let dir = std::env::temp_dir().join("cadr-studio-updates");
    let mut failure = None;
    for entry in std::fs::read_dir(&dir).ok()?.flatten() {
        let path = entry.path();
        let name = entry.file_name().to_string_lossy().into_owned();
        if !(name.starts_with("apply-update-") && name.ends_with(STATUS_SUFFIX)) {
            continue;
        }
        let Ok(text) = std::fs::read_to_string(&path) else { continue };
        let started_only = text.trim().trim_start_matches('\u{feff}').trim() == "STARTED";
        if started_only {
            let age = entry.metadata().ok().and_then(|m| m.modified().ok()).and_then(|t| t.elapsed().ok());
            if age.is_none_or(|a| a.as_secs() < STARTED_STALE_SECS) {
                continue;
            }
        }
        let _ = std::fs::remove_file(&path);
        if let Some(f) = parse_update_status(&text) {
            failure = Some(f);
        }
    }
    failure
}

/// Interpret the status file text (see [`take_last_update_failure`]).
pub fn parse_update_status(text: &str) -> Option<String> {
    let t = text.trim().trim_start_matches('\u{feff}').trim();
    if let Some(rest) = t.strip_prefix("FAILED:") {
        let msg: String = rest.trim().chars().take(300).collect();
        Some(if msg.is_empty() { "the update could not be applied".to_string() } else { msg })
    } else if t == "STARTED" {
        Some("the update helper started but did not finish".to_string())
    } else {
        None
    }
}

/// Parse the value of a registry string entry from the stdout of `reg query`.
pub fn parse_reg_query_value(output: &str, value_name: &str) -> Option<String> {
    for line in output.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with("HKEY_") || trimmed.starts_with("ERROR:") {
            continue;
        }
        let mut tokens = trimmed.split_whitespace();
        let Some(name) = tokens.next() else { continue };
        if !name.eq_ignore_ascii_case(value_name) {
            continue;
        }
        let Some(reg_type) = tokens.next() else { continue };
        if !reg_type.starts_with("REG_") {
            continue;
        }
        let low_line = line.to_ascii_lowercase();
        let name_idx = low_line.find(&name.to_ascii_lowercase())?;
        let after_name = &line[name_idx + name.len()..];
        let low_after = after_name.to_ascii_lowercase();
        let type_idx = low_after.find(&reg_type.to_ascii_lowercase())?;
        let val = after_name[type_idx + reg_type.len()..].trim();
        return Some(val.to_string());
    }
    None
}

/// Parse the `InstallDir` value from `reg query HKLM\Software\FilmCraft /v InstallDir` output.
pub fn parse_reg_query_install_dir(output: &str) -> Option<String> {
    parse_reg_query_value(output, "InstallDir")
}

/// Query the registry for the HKLM\Software\FilmCraft `InstallDir` entry.
#[cfg(windows)]
pub fn read_msi_install_dir_from_registry() -> Option<String> {
    use std::os::windows::process::CommandExt;
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    let output = std::process::Command::new("reg.exe")
        .args(["query", r"HKLM\Software\FilmCraft", "/v", "InstallDir"])
        .creation_flags(CREATE_NO_WINDOW)
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&output.stdout);
    parse_reg_query_install_dir(&text)
}

#[cfg(not(windows))]
pub fn read_msi_install_dir_from_registry() -> Option<String> {
    None
}

/// Compare the application directory with the registry `InstallDir` value.
/// Returns true when they point to the same directory (case-insensitive, ignoring trailing slashes).
pub fn is_msi_managed_install(exe_dir: &std::path::Path, registry_install_dir: &str) -> bool {
    let reg_trimmed = registry_install_dir.trim();
    if reg_trimmed.is_empty() {
        return false;
    }
    let exe_str = exe_dir.to_string_lossy();
    let norm_exe = normalize_path_for_comparison(&exe_str);
    let norm_reg = normalize_path_for_comparison(reg_trimmed);
    !norm_exe.is_empty() && norm_exe.eq_ignore_ascii_case(&norm_reg)
}

fn normalize_path_for_comparison(p: &str) -> String {
    let s = p.trim().replace('/', "\\");
    s.trim_end_matches('\\').to_string()
}

/// Returns true if the currently running binary resides in an MSI-managed installation directory.
pub fn is_current_install_msi_managed() -> bool {
    #[cfg(windows)]
    {
        let Ok(current_exe) = std::env::current_exe() else { return false };
        let Some(exe_dir) = current_exe.parent() else { return false };
        let Some(reg_dir) = read_msi_install_dir_from_registry() else { return false };
        is_msi_managed_install(exe_dir, &reg_dir)
    }
    #[cfg(not(windows))]
    {
        false
    }
}

/// Return the command-line arguments for msiexec to run an elevated all-users upgrade.
pub fn build_msiexec_args(msi_path: &str, log_path: &str) -> Vec<String> {
    vec![
        "/i".to_string(),
        msi_path.to_string(),
        "ALLUSERS=1".to_string(),
        "/passive".to_string(),
        "/norestart".to_string(),
        "/L*v".to_string(),
        log_path.to_string(),
    ]
}

/// Quote one command-line argument for a Windows argument string when it contains whitespace or quotes.
pub fn quote_windows_arg(arg: &str) -> String {
    if arg.contains([' ', '\t', '"']) { format!("\"{}\"", arg.replace('"', "\\\"")) } else { arg.to_string() }
}

/// Launch msiexec elevated with UAC using PowerShell `Start-Process -Verb RunAs`.
#[cfg(windows)]
pub fn launch_windows_msi_upgrade(staged_msi: &std::path::Path) -> Result<(), String> {
    use std::os::windows::process::CommandExt;

    if !staged_msi.is_file() {
        return Err("staged MSI installer does not exist".into());
    }
    // Re-hash right before launch; fail closed (and delete the file) on any mismatch.
    verify_staged_msi_before_launch(staged_msi)?;

    let temp_dir = std::env::temp_dir().join("cadr-studio-updates");
    std::fs::create_dir_all(&temp_dir).map_err(|e| format!("failed creating update staging folder: {e}"))?;
    let pid = std::process::id();
    let log_path = temp_dir.join(format!("msi-upgrade-{pid}.log"));

    let msi_path_str = staged_msi.to_string_lossy();
    let log_path_str = log_path.to_string_lossy();

    let args = build_msiexec_args(&msi_path_str, &log_path_str);
    let arg_line = args.iter().map(|a| quote_windows_arg(a)).collect::<Vec<_>>().join(" ");
    let quoted = ps_single_quote(&arg_line);
    let ps_cmd = format!("$ErrorActionPreference = 'Stop'; Start-Process -FilePath 'msiexec.exe' -ArgumentList {quoted} -Verb RunAs");

    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    let output = std::process::Command::new("powershell.exe")
        .args(["-NoProfile", "-NonInteractive", "-ExecutionPolicy", "Bypass", "-WindowStyle", "Hidden", "-Command", &ps_cmd])
        .creation_flags(CREATE_NO_WINDOW)
        .output()
        .map_err(|e| format!("failed launching elevated MSI installer: {e}"))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
        let stdout = String::from_utf8_lossy(&output.stdout).trim().to_string();
        let raw = if !stderr.is_empty() {
            stderr
        } else if !stdout.is_empty() {
            stdout
        } else {
            "administrator permission was declined or the installer failed to start".to_string()
        };
        let first_line = raw.lines().next().unwrap_or(&raw).trim().chars().take(200).collect::<String>();
        return Err(format!("MSI upgrade failed: {first_line}"));
    }

    Ok(())
}

#[cfg(not(windows))]
pub fn launch_windows_msi_upgrade(staged_msi: &std::path::Path) -> Result<(), String> {
    Err(format!("MSI upgrade is supported on Windows; staged file saved at {}", staged_msi.display()))
}

/// Shown instead of attempting a self-replace that cannot succeed (for example an all-users install
/// under Program Files, where a normal account has no write access). No elevation is attempted.
pub const PROTECTED_INSTALL_MESSAGE: &str = "CADR Studio is installed in a protected folder (for example Program Files), so it cannot replace itself without administrator rights. Nothing was changed. Install the new version with the MSI installer from https://github.com/Madboly1/CADR-EDIT/releases/latest";

/// Whether the current account can create (and remove) a file in `dir`. Used to decide up front
/// whether the in-place update can work; the probe file is removed immediately.
pub fn dir_is_writable(dir: &std::path::Path) -> bool {
    let probe = dir.join(format!(".cadr-write-probe-{}", std::process::id()));
    match std::fs::OpenOptions::new().write(true).create_new(true).open(&probe) {
        Ok(_) => {
            let _ = std::fs::remove_file(&probe);
            true
        }
        Err(_) => false,
    }
}

/// Launch the detached Windows updater helper script to replace the running executable and
/// relaunch CADR Studio once this process exits.
#[cfg(windows)]
pub fn launch_windows_self_replace(staged_exe: &std::path::Path) -> Result<(), String> {
    use std::os::windows::process::CommandExt;

    if !staged_exe.is_file() {
        return Err("staged update executable does not exist".into());
    }
    if staged_exe.extension().and_then(|e| e.to_str()).is_some_and(|e| e.eq_ignore_ascii_case("msi")) {
        return launch_windows_msi_upgrade(staged_exe);
    }
    let target_exe = std::env::current_exe().map_err(|e| format!("failed locating current executable: {e}"))?;
    if let Some(dir) = target_exe.parent()
        && !dir_is_writable(dir)
    {
        return Err(PROTECTED_INSTALL_MESSAGE.to_string());
    }
    let pid = std::process::id();
    let script_path = std::env::temp_dir().join("cadr-studio-updates").join(format!("apply-update-{pid}.ps1"));
    if let Some(parent) = script_path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| format!("failed creating updater script folder: {e}"))?;
    }
    let script = build_windows_update_script(pid, &staged_exe.to_string_lossy(), &target_exe.to_string_lossy(), &script_path.to_string_lossy());
    std::fs::write(&script_path, script).map_err(|e| format!("failed writing updater script: {e}"))?;

    // CREATE_NO_WINDOW (0x0800_0000): run cleanly in the background without flashing a console.
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    std::process::Command::new("powershell.exe")
        .args(["-NoProfile", "-NonInteractive", "-ExecutionPolicy", "Bypass", "-WindowStyle", "Hidden", "-File"])
        .arg(&script_path)
        .creation_flags(CREATE_NO_WINDOW)
        .spawn()
        .map_err(|e| format!("failed launching Windows updater script: {e}"))?;
    Ok(())
}

/// Non-Windows fallback: reports the staged file path cleanly without failing compilation.
#[cfg(not(windows))]
pub fn launch_windows_self_replace(staged_exe: &std::path::Path) -> Result<(), String> {
    if staged_exe.extension().and_then(|e| e.to_str()).is_some_and(|e| e.eq_ignore_ascii_case("msi")) {
        return launch_windows_msi_upgrade(staged_exe);
    }
    Err(format!("automatic .exe replacement is supported on Windows; staged file saved at {}", staged_exe.display()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use filmcraft_media::mogrt::build_zip;
    use serde_json::json;

    #[test]
    fn semver_parsing_and_comparison() {
        assert!(is_newer_version("0.20.1", "v0.20.2"));
        assert!(is_newer_version("0.20.1", "v0.21.0"));
        assert!(is_newer_version("0.20.1", "1.0.0"));
        assert!(is_newer_version("0.20.2-rc.1", "v0.20.2"));
        assert!(is_newer_version("0.20.1", "v0.20.2-rc.1"));

        assert!(!is_newer_version("0.20.1", "v0.20.1"));
        assert!(!is_newer_version("0.20.2", "v0.20.1"));
        assert!(!is_newer_version("0.20.2", "v0.20.2-rc.1"));
        assert!(!is_newer_version("0.20.1", "not-a-version"));
        assert_eq!(parse_semver(""), None);
    }

    #[test]
    fn parses_github_release_and_prefers_windows_exe_asset() {
        let payload = serde_json::to_vec(&json!({
            "tag_name": "v0.21.0",
            "name": "CADR Studio 0.21.0",
            "body": "- Native .mogrt support\n- In-app GitHub Auto-Updater",
            "html_url": "https://github.com/Madboly1/CADR-EDIT/releases/tag/v0.21.0",
            "assets": [
                {
                    "name": "filmcraft-0.21.0-windows-x64-portable.zip",
                    "browser_download_url": "https://github.com/Madboly1/CADR-EDIT/releases/download/v0.21.0/filmcraft-0.21.0-windows-x64-portable.zip",
                    "size": 52_000_000
                },
                {
                    "name": "CADR-Studio-windows-x86_64.exe",
                    "browser_download_url": "https://github.com/Madboly1/CADR-EDIT/releases/download/v0.21.0/CADR-Studio-windows-x86_64.exe",
                    "size": 48_000_000
                }
            ]
        }))
        .unwrap();

        match parse_github_release_json(&payload, "0.20.1").unwrap() {
            UpdateCheckResult::UpdateAvailable(info) => {
                assert_eq!(info.tag_name, "v0.21.0");
                assert_eq!(info.version, "0.21.0");
                assert_eq!(info.asset_name.as_deref(), Some("CADR-Studio-windows-x86_64.exe"));
                assert!(!info.is_zip);
                assert_eq!(info.asset_size, 48_000_000);
                assert!(info.release_notes.contains("Native .mogrt support"));
            }
            other => panic!("expected UpdateAvailable, got {other:?}"),
        }

        // Same version reports UpToDate
        match parse_github_release_json(&payload, "0.21.0").unwrap() {
            UpdateCheckResult::UpToDate { current, latest } => {
                assert_eq!(current, "0.21.0");
                assert_eq!(latest, "0.21.0");
            }
            other => panic!("expected UpToDate, got {other:?}"),
        }
    }

    #[test]
    fn falls_back_to_windows_zip_and_extracts_valid_pe() {
        let payload = serde_json::to_vec(&json!({
            "tag_name": "v0.22.0",
            "name": "CADR Studio 0.22.0",
            "body": "ZIP release",
            "assets": [
                {
                    "name": "CADR-Studio-windows-x86_64.zip",
                    "browser_download_url": "https://github.com/Madboly1/CADR-EDIT/releases/download/v0.22.0/CADR-Studio-windows-x86_64.zip",
                    "size": 50_000_000
                }
            ]
        }))
        .unwrap();
        let UpdateCheckResult::UpdateAvailable(info) = parse_github_release_json(&payload, "0.20.1").unwrap() else { panic!("expected update") };
        assert!(info.is_zip);

        let fake_exe = b"MZ\x90\x00fake-pe-payload";
        let zip = build_zip(&[("CADR-Studio/filmcraft.exe", fake_exe), ("CADR-Studio/README.md", b"hi")], true);
        let extracted = extract_exe_from_zip(&zip).unwrap();
        assert_eq!(extracted, fake_exe);

        // Reject non-PE executable inside ZIP
        let bad_zip = build_zip(&[("filmcraft.exe", b"NOT_A_PE_BINARY")], false);
        assert!(extract_exe_from_zip(&bad_zip).is_err());
    }

    #[test]
    fn windows_update_script_escapes_paths_and_waits_for_pid() {
        let script = build_windows_update_script(4242, r"C:\Temp\cadr-update.exe", r"H:\O'Connor Studio\CADR-Studio.exe", r"C:\Temp\apply-update-4242.ps1");
        assert!(script.contains("$pidToWait = 4242"));
        assert!(script.contains("'H:\\O''Connor Studio\\CADR-Studio.exe'"));
        assert!(script.contains("Wait-Process -Id $pidToWait"));
        assert!(script.contains("Start-Process -FilePath $target"));
    }

    #[test]
    fn update_status_file_parsing() {
        assert_eq!(parse_update_status("OK\r\n"), None);
        assert_eq!(parse_update_status(""), None);
        assert_eq!(parse_update_status("\u{feff}FAILED: could not replace\r\n").as_deref(), Some("could not replace"));
        assert!(parse_update_status("STARTED").is_some());
        assert!(parse_update_status("FAILED:").is_some());
    }

    #[test]
    fn update_script_reports_status_and_relaunches_only_on_success() {
        let script = build_windows_update_script(1, r"C:\t\s.exe", r"C:\a\CADR-Studio.exe", r"C:\t\apply.ps1");
        assert!(script.contains(STATUS_SUFFIX));
        assert!(!script.contains("Exception.Message"));
        assert!(script.contains("Write-Status 'OK'"));
        assert!(script.contains("FAILED:"));
        let ok = script.find("Write-Status 'OK'").unwrap();
        let start = script.find("Start-Process").unwrap();
        assert!(ok < start);
        assert!(!script[ok..start].contains("FAILED"));
    }

    #[test]
    fn writable_probe_detects_missing_and_usable_dirs() {
        let dir = std::env::temp_dir().join(format!("cadr-probe-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        assert!(dir_is_writable(&dir));
        assert_eq!(std::fs::read_dir(&dir).unwrap().count(), 0, "probe file must be removed");
        std::fs::remove_dir_all(&dir).unwrap();
        assert!(!dir_is_writable(&dir), "a missing dir is not writable");
    }

    const DIGEST_ABC: &str = "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad";
    const DIGEST_EMPTY: &str = "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855";

    fn msi_url(file: &str) -> String {
        format!("{}v0.22.0/{file}", release_download_prefix())
    }

    fn msi_release(url: Option<String>, sha256: Option<&str>) -> ReleaseInfo {
        ReleaseInfo { msi_download_url: url, msi_sha256: sha256.map(str::to_string), ..Default::default() }
    }

    #[test]
    fn msi_asset_parsing() {
        let url = msi_url("filmcraft-0.22.0-windows-x64.msi");
        let payload = serde_json::to_vec(&json!({
            "tag_name": "v0.22.0",
            "name": "CADR Studio 0.22.0",
            "assets": [
                {"name": "filmcraft-0.22.0-windows-arm64.msi", "browser_download_url": msi_url("filmcraft-0.22.0-windows-arm64.msi"), "size": 5},
                {"name": "CADR-Studio-windows-x86_64.exe", "browser_download_url": msi_url("CADR-Studio-windows-x86_64.exe"), "size": 48_000_000},
                {"name": "filmcraft-0.22.0-windows-x64.msi", "browser_download_url": url, "size": 55_000_000, "digest": format!("sha256:{DIGEST_ABC}")}
            ]
        }))
        .unwrap();
        let UpdateCheckResult::UpdateAvailable(info) = parse_github_release_json(&payload, "0.20.1").unwrap() else { panic!("expected update") };
        // The exe selection is unchanged and the MSI is recorded separately.
        assert_eq!(info.asset_name.as_deref(), Some("CADR-Studio-windows-x86_64.exe"));
        assert_eq!(info.asset_size, 48_000_000);
        assert_eq!(info.msi_asset_name.as_deref(), Some("filmcraft-0.22.0-windows-x64.msi"));
        assert_eq!(info.msi_download_url.as_deref(), Some(url.as_str()));
        assert_eq!(info.msi_asset_size, 55_000_000);
        assert_eq!(info.msi_sha256.as_deref(), Some(DIGEST_ABC));
    }

    #[test]
    fn msi_asset_requires_trusted_url() {
        let evil = json!({"name": "a-windows-x64.msi", "browser_download_url": "https://evil.example.com/a-windows-x64.msi", "size": 1});
        let other_repo = json!({"name": "b-windows-x64.msi", "browser_download_url": "https://github.com/someone/else/releases/download/v1/b-windows-x64.msi", "size": 1});
        let plain_http = json!({"name": "c-windows-x64.msi", "browser_download_url": "http://github.com/Madboly1/CADR-EDIT/releases/download/v1/c-windows-x64.msi", "size": 1});
        assert!(pick_windows_msi_asset(&[evil, other_repo, plain_http]).is_none());
    }

    #[test]
    fn msi_digest_missing_or_malformed_is_not_recorded() {
        let url = msi_url("a-windows-x64.msi");
        let missing = json!({"name": "a-windows-x64.msi", "browser_download_url": url, "size": 1});
        let short = json!({"name": "a-windows-x64.msi", "browser_download_url": url, "size": 1, "digest": "sha256:abc"});
        let wrong_algo = json!({"name": "a-windows-x64.msi", "browser_download_url": url, "size": 1, "digest": format!("sha1:{DIGEST_ABC}")});
        assert_eq!(pick_windows_msi_asset(&[missing]).unwrap().sha256, None);
        assert_eq!(pick_windows_msi_asset(&[short]).unwrap().sha256, None);
        assert_eq!(pick_windows_msi_asset(&[wrong_algo]).unwrap().sha256, None);
    }

    #[test]
    fn trusted_release_url_prefix() {
        assert_eq!(release_download_prefix(), "https://github.com/Madboly1/CADR-EDIT/releases/download/");
        assert!(is_trusted_release_url("https://github.com/Madboly1/CADR-EDIT/releases/download/v1/a.msi"));
        assert!(!is_trusted_release_url("http://github.com/Madboly1/CADR-EDIT/releases/download/v1/a.msi"));
        assert!(!is_trusted_release_url("https://github.com/Madboly1/CADR-EDIT/releases/latest"));
        assert!(!is_trusted_release_url("https://github.com/Madboly1/CADR-EDIT-evil/releases/download/v1/a.msi"));
        assert!(!is_trusted_release_url("https://github.com.evil.example/Madboly1/CADR-EDIT/releases/download/v1/a.msi"));
    }

    #[test]
    fn msi_download_requirements_fail_closed() {
        let url = msi_url("a-windows-x64.msi");
        let ok = msi_release(Some(url.clone()), Some(DIGEST_ABC));
        assert_eq!(msi_download_requirements(&ok), Ok((url.as_str(), DIGEST_ABC.to_string())));
        assert!(msi_download_requirements(&msi_release(Some(url.clone()), None)).is_err());
        assert!(msi_download_requirements(&msi_release(Some(url.clone()), Some("abc"))).is_err());
        assert!(msi_download_requirements(&msi_release(Some("https://evil.example.com/a.msi".to_string()), Some(DIGEST_ABC))).is_err());
        assert!(msi_download_requirements(&msi_release(None, Some(DIGEST_ABC))).is_err());
    }

    #[test]
    fn msi_install_detection_rules() {
        use std::path::Path;

        let exe_canonical = Path::new(r"C:\Program Files\CADR\CADR Studio");
        let reg_canonical = r"C:\Program Files\CADR\CADR Studio";
        assert!(is_msi_managed_install(exe_canonical, reg_canonical));

        let exe_portable = Path::new(r"C:\Users\User\Downloads\CADR Studio");
        assert!(!is_msi_managed_install(exe_portable, reg_canonical));
        let exe_other_program = Path::new(r"C:\Program Files\OtherApp");
        assert!(!is_msi_managed_install(exe_other_program, reg_canonical));

        let exe_trailing = Path::new(r"C:\Program Files\CADR\CADR Studio\");
        let reg_trailing = r"C:\Program Files\CADR\CADR Studio\";
        assert!(is_msi_managed_install(exe_trailing, reg_canonical));
        assert!(is_msi_managed_install(exe_canonical, reg_trailing));
        assert!(is_msi_managed_install(exe_trailing, reg_trailing));

        let exe_forward = Path::new("C:/Program Files/CADR/CADR Studio/");
        assert!(is_msi_managed_install(exe_forward, reg_canonical));

        let exe_lower = Path::new(r"c:\program files\cadr\cadr studio");
        let reg_upper = r"C:\PROGRAM FILES\CADR\CADR STUDIO";
        assert!(is_msi_managed_install(exe_lower, reg_upper));

        assert!(!is_msi_managed_install(exe_canonical, ""));
        assert!(!is_msi_managed_install(exe_canonical, "   "));
    }

    #[test]
    fn reg_output_parser() {
        let reg_out = "\r\nHKEY_LOCAL_MACHINE\\Software\\FilmCraft\r\n    InstallDir    REG_SZ    C:\\Program Files\\CADR\\CADR Studio\r\n";
        assert_eq!(parse_reg_query_install_dir(reg_out), Some(r"C:\Program Files\CADR\CADR Studio".to_string()));

        let reg_out_expand = "\tInstallDir\tREG_EXPAND_SZ\tC:\\Program Files\\CADR\\CADR Studio\\\r\n";
        assert_eq!(parse_reg_query_install_dir(reg_out_expand), Some(r"C:\Program Files\CADR\CADR Studio\".to_string()));

        let reg_err = "ERROR: The system was unable to find the specified registry key or value.\r\n";
        assert_eq!(parse_reg_query_install_dir(reg_err), None);
        let reg_other = "    DisplayName    REG_SZ    CADR Studio\r\n";
        assert_eq!(parse_reg_query_install_dir(reg_other), None);
        assert_eq!(parse_reg_query_install_dir(""), None);
    }

    #[test]
    fn build_msiexec_args_exact_vector() {
        let msi = r"C:\Users\Alice\AppData\Local\Temp\cadr-studio-updates\cadr-update-0.21.0-42.msi";
        let log = r"C:\Users\Alice\AppData\Local\Temp\cadr-studio-updates\msi-upgrade-42.log";
        let expected = ["/i", msi, "ALLUSERS=1", "/passive", "/norestart", "/L*v", log].map(String::from);
        assert_eq!(build_msiexec_args(msi, log), expected.to_vec());
    }

    #[test]
    fn windows_arg_quoting() {
        assert_eq!(quote_windows_arg("/passive"), "/passive");
        assert_eq!(quote_windows_arg(r"C:\Temp\my file.msi"), r#""C:\Temp\my file.msi""#);
    }

    #[test]
    fn msi_magic_validation() {
        let mut valid_magic = MSI_COMPOUND_MAGIC.to_vec();
        valid_magic.extend_from_slice(b"additional-data-for-compound-document");
        assert!(validate_msi_bytes(&valid_magic).is_ok());

        let exe_buf = b"MZ\x90\x00\x03\x00\x00\x00\x04\x00\x00\x00\xff\xff\x00\x00fake-pe";
        assert!(validate_msi_bytes(exe_buf).is_err());

        let short_buf = &[0xD0, 0xCF, 0x11, 0xE0];
        assert!(validate_msi_bytes(short_buf).is_err());
        assert!(validate_msi_bytes(&[]).is_err());
    }

    #[test]
    fn sha256_known_vectors() {
        assert_eq!(sha256_bytes_hex(b""), DIGEST_EMPTY);
        assert_eq!(sha256_bytes_hex(b"abc"), DIGEST_ABC);
    }

    #[test]
    fn sha256_digest_parsing() {
        assert_eq!(parse_sha256_digest(&format!("sha256:{DIGEST_ABC}")).as_deref(), Some(DIGEST_ABC));
        assert_eq!(parse_sha256_digest(&format!("sha256:{}", DIGEST_ABC.to_ascii_uppercase())).as_deref(), Some(DIGEST_ABC));
        assert_eq!(parse_sha256_digest("sha256:abc"), None);
        assert_eq!(parse_sha256_digest(&format!("sha1:{DIGEST_ABC}")), None);
        assert_eq!(parse_sha256_digest(DIGEST_ABC), None);
        assert_eq!(parse_sha256_digest(&format!("sha256:{}z", &DIGEST_ABC[..63])), None);
        assert_eq!(parse_sha256_digest(&format!("sha256:{DIGEST_ABC}00")), None);
        assert_eq!(parse_sha256_digest(""), None);
        assert_eq!(normalize_sha256_hex(DIGEST_ABC).as_deref(), Some(DIGEST_ABC));
        assert_eq!(normalize_sha256_hex("not-hex"), None);
    }

    #[test]
    fn file_hash_verification_deletes_on_mismatch() {
        let dir = tempfile::tempdir().unwrap();
        let good = dir.path().join("good.msi");
        std::fs::write(&good, b"abc").unwrap();
        assert_eq!(sha256_file_hex(&good).unwrap(), DIGEST_ABC);
        assert!(verify_file_sha256(&good, DIGEST_ABC).is_ok());
        assert!(good.exists());

        let bad = dir.path().join("bad.msi");
        std::fs::write(&bad, b"abd").unwrap();
        assert!(verify_file_sha256(&bad, DIGEST_ABC).is_err());
        assert!(!bad.exists(), "a mismatching file must be deleted");

        let malformed = dir.path().join("malformed.msi");
        std::fs::write(&malformed, b"abc").unwrap();
        assert!(verify_file_sha256(&malformed, "not-hex").is_err());
        assert!(!malformed.exists());

        assert!(verify_file_sha256(&dir.path().join("missing.msi"), DIGEST_ABC).is_err());
    }

    #[test]
    fn pre_launch_verification_fails_closed() {
        let dir = tempfile::tempdir().unwrap();

        let unrecorded = dir.path().join("unrecorded.msi");
        std::fs::write(&unrecorded, b"abc").unwrap();
        assert!(verify_staged_msi_before_launch(&unrecorded).is_err());
        assert!(!unrecorded.exists(), "an unverified file must be deleted");

        let staged = dir.path().join("staged.msi");
        std::fs::write(&staged, b"abc").unwrap();
        record_verified_msi(&staged, DIGEST_ABC);
        assert_eq!(recorded_msi_digest(&staged).as_deref(), Some(DIGEST_ABC));
        assert!(verify_staged_msi_before_launch(&staged).is_ok());

        std::fs::write(&staged, b"swapped").unwrap();
        assert!(verify_staged_msi_before_launch(&staged).is_err());
        assert!(!staged.exists(), "a swapped file must be deleted");
    }
}
