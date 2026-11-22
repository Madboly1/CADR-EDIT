//! Storage for user-supplied API keys (BYOK).
//!
//! Secrets are NEVER written to `preferences.json`, NEVER recorded in `session.journal`, NEVER
//! hardcoded, and NEVER logged.
//!
//! Platform backends:
//! - **Windows (`target_os = "windows"`)**: Native Win32 DPAPI (`CryptProtectData` /
//!   `CryptUnprotectData` in `crypt32.dll`, `CRYPTPROTECT_UI_FORBIDDEN`, current-user scope; the
//!   FFI lives in `filmcraft_platform::dpapi`, the one crate allowed to use `unsafe`).
//!   Protected blobs are persisted under `<data_dir>/secrets.json` with prefix `cadr.dpapi.v1:`
//!   when a data directory is attached.
//! - **macOS, Linux, and other OSes (and sessions without a data directory)**: **Session-only
//!   (in-memory)** — secrets are held only in process memory for the current session and are NEVER
//!   written to disk (no `secrets.json`, no `.secret.salt`, and no CLI subprocesses such as
//!   `/usr/bin/security` that could expose keys in the process list).

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

#[cfg(target_os = "windows")]
use base64::Engine as _;
#[cfg(target_os = "windows")]
use base64::engine::general_purpose::STANDARD as B64;

/// Secret key name for the user's Gemini BYOK API key.
pub const GEMINI_API_KEY: &str = "geminiApiKey";

#[cfg(target_os = "windows")]
const SECRETS_FILE: &str = "secrets.json";

#[cfg(target_os = "windows")]
const DPAPI_PREFIX: &str = "cadr.dpapi.v1:";

/// Secret store bound to an optional data directory.
///
/// Persists via Windows DPAPI on Windows when `dir` is set; strictly session-only (in-memory,
/// never written to disk) on all other platforms or when `dir` is `None`.
#[derive(Clone, Debug, Default)]
pub struct SecretStore {
    dir: Option<PathBuf>,
    /// In-memory session-only secrets (used on non-Windows platforms and ephemeral sessions).
    memory: BTreeMap<String, String>,
}

impl SecretStore {
    /// Store associated with `dir` (persisted via DPAPI on Windows; session-only elsewhere).
    pub fn for_dir(dir: PathBuf) -> Self {
        Self { dir: Some(dir), memory: BTreeMap::new() }
    }

    /// Rebind the store to a data directory (called when `Session::start_autosave` attaches one).
    pub fn set_dir(&mut self, dir: PathBuf) {
        self.dir = Some(dir);
    }

    /// Data directory associated with this store, if any.
    pub fn dir(&self) -> Option<&Path> {
        self.dir.as_deref()
    }

    /// Whether secrets saved in this store persist across restarts using an OS credential store.
    ///
    /// True only on Windows with an attached data directory (DPAPI); false on macOS, Linux, and
    /// other platforms where keys are kept strictly for the current session in memory.
    pub fn is_persistent(&self) -> bool {
        cfg!(target_os = "windows") && self.dir.is_some()
    }

    /// Human-readable description of the active storage backend for the Settings UI.
    pub fn backend_label(&self) -> &'static str {
        if self.is_persistent() { "Windows DPAPI (current user)" } else { "Session only (in memory, not saved to disk)" }
    }

    /// Store (or delete, if `value` is empty/whitespace) a secret.
    pub fn set(&mut self, name: &str, value: &str) -> Result<(), String> {
        let trimmed = value.trim();
        if trimmed.is_empty() {
            return self.delete(name);
        }
        #[cfg(target_os = "windows")]
        if let Some(dir) = self.dir.clone() {
            let blob = filmcraft_platform::dpapi::protect(trimmed.as_bytes())?;
            let encoded = format!("{DPAPI_PREFIX}{}", B64.encode(blob));
            let mut map = read_secrets_map(&dir)?;
            map.insert(name.to_string(), encoded);
            write_secrets_map(&dir, &map)?;
            self.memory.remove(name);
            return Ok(());
        }
        // Session-only: keep in memory only, never write to disk.
        self.memory.insert(name.to_string(), trimmed.to_string());
        Ok(())
    }

    /// Remove a secret from the active store.
    pub fn delete(&mut self, name: &str) -> Result<(), String> {
        #[cfg(target_os = "windows")]
        if let Some(dir) = self.dir.clone() {
            let mut map = read_secrets_map(&dir)?;
            if map.remove(name).is_some() {
                write_secrets_map(&dir, &map)?;
            }
        }
        self.memory.remove(name);
        Ok(())
    }

    /// Read a secret. Returns `Ok(None)` if not configured.
    pub fn get(&self, name: &str) -> Result<Option<String>, String> {
        #[cfg(target_os = "windows")]
        if let Some(dir) = &self.dir {
            let map = read_secrets_map(dir)?;
            let Some(token) = map.get(name) else {
                return Ok(None);
            };
            if token.is_empty() {
                return Ok(None);
            }
            let Some(rest) = token.strip_prefix(DPAPI_PREFIX) else {
                return Err("stored secret uses an unrecognized or incompatible encryption format; re-enter it in Settings".into());
            };
            let raw = B64.decode(rest).map_err(|_| "corrupted DPAPI secret encoding".to_string())?;
            let plain = filmcraft_platform::dpapi::unprotect(&raw)?;
            return String::from_utf8(plain).map(Some).map_err(|_| "stored secret is not valid UTF-8".to_string());
        }
        Ok(self.memory.get(name).filter(|s| !s.is_empty()).cloned())
    }

    /// Whether a non-empty secret is stored under `name`.
    pub fn is_configured(&self, name: &str) -> bool {
        self.get(name).ok().flatten().is_some_and(|s| !s.is_empty())
    }

    /// Redacted preview suitable for UI status display (never exposes the full key).
    pub fn masked(&self, name: &str) -> String {
        match self.get(name) {
            Ok(Some(k)) if !k.is_empty() => mask_secret(&k),
            _ => String::new(),
        }
    }
}

/// Mask a secret for UI display without revealing the full key.
pub fn mask_secret(secret: &str) -> String {
    let chars: Vec<char> = secret.chars().collect();
    if chars.len() <= 8 {
        return "••••••••".into();
    }
    let prefix: String = chars[..4].iter().collect();
    let suffix: String = chars[chars.len() - 4..].iter().collect();
    format!("{prefix}••••{suffix}")
}

// ------------------------------------------------------------------ Windows DPAPI

#[cfg(target_os = "windows")]
fn read_secrets_map(dir: &Path) -> Result<BTreeMap<String, String>, String> {
    let path = dir.join(SECRETS_FILE);
    match std::fs::read_to_string(&path) {
        Ok(text) => serde_json::from_str(&text).map_err(|e| format!("invalid secrets store: {e}")),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(BTreeMap::new()),
        Err(e) => Err(format!("could not read secrets store: {e}")),
    }
}

#[cfg(target_os = "windows")]
fn write_secrets_map(dir: &Path, map: &BTreeMap<String, String>) -> Result<(), String> {
    std::fs::create_dir_all(dir).map_err(|e| format!("could not create secrets directory: {e}"))?;
    let json = serde_json::to_string_pretty(map).map_err(|e| format!("could not serialize secrets store: {e}"))?;
    let path = dir.join(SECRETS_FILE);
    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, json.as_bytes()).map_err(|e| format!("could not write private file: {e}"))?;
    std::fs::rename(&tmp, &path).map_err(|e| format!("could not finalize private file: {e}"))
}
