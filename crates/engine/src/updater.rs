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
}

/// Outcome of checking the latest GitHub release against the running binary version.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum UpdateCheckResult {
    UpToDate {
        current: String,
        latest: String,
    },
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

/// Parse a GitHub `/releases/latest` JSON response and compare it to `current_version`.
pub fn parse_github_release_json(bytes: &[u8], current_version: &str) -> Result<UpdateCheckResult, String> {
    let root: Value = serde_json::from_slice(bytes).map_err(|e| format!("invalid GitHub release JSON: {e}"))?;
    let tag_name = root.get("tag_name").and_then(Value::as_str).filter(|s| !s.trim().is_empty()).ok_or_else(|| "release JSON missing `tag_name`".to_string())?;
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
    let exe_bytes = filmcraft_media::mogrt::extract_zip_entry(zip_bytes, pick, 256 * 1024 * 1024).map_err(|e| format!("failed extracting {}: {e}", pick.name))?;
    validate_windows_exe_bytes(&exe_bytes, 2)?;
    Ok(exe_bytes)
}

/// Download the release asset for `release` into the OS temp directory with progress updates.
///
/// `on_progress(downloaded, total)` returns `false` to cancel the download. Returns the verified
/// path to the staged `.exe` ready for self-replacement.
#[cfg(not(target_arch = "wasm32"))]
pub fn download_release_asset(release: &ReleaseInfo, on_progress: &mut dyn FnMut(u64, u64) -> bool) -> Result<PathBuf, String> {
    use std::io::{Read, Write};

    let url = release.download_url.as_deref().filter(|u| u.starts_with("https://")).ok_or_else(|| "release has no downloadable Windows asset".to_string())?;
    let stage_dir = std::env::temp_dir().join("cadr-studio-updates");
    std::fs::create_dir_all(&stage_dir).map_err(|e| format!("failed creating update staging folder: {e}"))?;

    let safe_ver: String = release.version.chars().map(|c| if c.is_ascii_alphanumeric() || c == '.' || c == '-' { c } else { '_' }).take(32).collect();
    let ext = if release.is_zip { "zip" } else { "exe" };
    let pid = std::process::id();
    let part_path = stage_dir.join(format!("cadr-update-{safe_ver}-{pid}.{ext}.part"));
    let final_dl = stage_dir.join(format!("cadr-update-{safe_ver}-{pid}.{ext}"));

    let agent = http_agent();
    let ua = format!("CADR-Studio/{CURRENT_VERSION}");
    let resp = agent.get(url).header("User-Agent", &ua).call().map_err(|e| format!("download failed: {e}"))?;
    let total = release.asset_size;
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
         try {{ Wait-Process -Id $pidToWait -Timeout 30 -ErrorAction SilentlyContinue }} catch {{}}\r\n\
         $replaced = $false\r\n\
         for ($i = 0; $i -lt 40; $i++) {{\r\n\
             try {{\r\n\
                 if (Test-Path -LiteralPath $backup) {{ Remove-Item -LiteralPath $backup -Force -ErrorAction SilentlyContinue }}\r\n\
                 if (Test-Path -LiteralPath $target) {{ Move-Item -LiteralPath $target -Destination $backup -Force }}\r\n\
                 Move-Item -LiteralPath $staged -Destination $target -Force\r\n\
                 $replaced = $true\r\n\
                 break\r\n\
             }} catch {{\r\n\
                 Start-Sleep -Milliseconds 250\r\n\
             }}\r\n\
         }}\r\n\
         if ($replaced) {{\r\n\
             Remove-Item -LiteralPath $backup -Force -ErrorAction SilentlyContinue\r\n\
             Start-Process -FilePath $target\r\n\
         }} elseif (Test-Path -LiteralPath $backup) {{\r\n\
             if (-not (Test-Path -LiteralPath $target)) {{ Move-Item -LiteralPath $backup -Destination $target -Force -ErrorAction SilentlyContinue }}\r\n\
         }}\r\n\
         Remove-Item -LiteralPath $staged -Force -ErrorAction SilentlyContinue\r\n\
         Remove-Item -LiteralPath $self -Force -ErrorAction SilentlyContinue\r\n"
    )
}

/// Launch the detached Windows updater helper script to replace the running executable and
/// relaunch CADR Studio once this process exits.
#[cfg(windows)]
pub fn launch_windows_self_replace(staged_exe: &std::path::Path) -> Result<(), String> {
    use std::os::windows::process::CommandExt;

    if !staged_exe.is_file() {
        return Err("staged update executable does not exist".into());
    }
    let target_exe = std::env::current_exe().map_err(|e| format!("failed locating current executable: {e}"))?;
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
}
