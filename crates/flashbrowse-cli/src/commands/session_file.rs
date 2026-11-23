//! On-disk persistent sessions (`~/.cache/flashbrowse/sessions/<id>.json`).
//!
//! `open --session-id <id>` detaches Chromium and writes this file;
//! `act`/`close` re-attach / kill by pid. Files are `0600` on Unix.

use std::fs;
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use flashbrowse_core::{DetachedSession, ResourcePolicy};
use serde::{Deserialize, Serialize};

/// JSON serializable session record.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PersistedSession {
    pub browser_ws_url: String,
    pub target_id: String,
    pub pid: u32,
    pub user_data_dir: String,
    /// Legacy field; use [`effective_resource_policy`] when reading old files.
    #[serde(default)]
    pub text_only: bool,
    #[serde(default)]
    pub resource_policy: Option<ResourcePolicy>,
    pub created_at: u64,
}

/// Policy stored in the session file (supports pre-policy JSON).
pub fn effective_resource_policy(rec: &PersistedSession) -> ResourcePolicy {
    rec.resource_policy
        .unwrap_or_else(|| ResourcePolicy::from_text_only(rec.text_only))
}

/// Root cache dir for flashbrowse state (`~/.cache/flashbrowse`).
pub use flashbrowse_core::paths::cache_dir as flashbrowse_cache_dir;

/// Cache sessions dir (platform-idiomatic).
pub fn sessions_dir() -> PathBuf {
    flashbrowse_core::paths::sessions_dir()
}

fn session_file(id: &str) -> PathBuf {
    sessions_dir().join(format!("{id}.json"))
}

/// Sanitize session id (alnum, dash, underscore only) to prevent traversal.
/// Validate a user-supplied session id (no path traversal).
pub fn validate_session_id(id: &str) -> anyhow::Result<String> {
    sanitized(id).ok_or_else(|| anyhow::anyhow!("invalid session id"))
}

fn sanitized(id: &str) -> Option<String> {
    if id.is_empty() || id.len() > 64 {
        return None;
    }
    if id
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
    {
        Some(id.to_string())
    } else {
        None
    }
}

pub fn write_session(id: &str, detached: &DetachedSession) -> anyhow::Result<()> {
    let id = sanitized(id).ok_or_else(|| anyhow::anyhow!("invalid session id"))?;
    let dir = sessions_dir();
    fs::create_dir_all(&dir)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = fs::set_permissions(&dir, fs::Permissions::from_mode(0o700));
    }
    let created_at = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let rec = PersistedSession {
        browser_ws_url: detached.browser_ws_url.clone(),
        target_id: detached.target_id.clone(),
        pid: detached.pid,
        user_data_dir: detached.user_data_dir.to_string_lossy().to_string(),
        text_only: detached.resource_policy == ResourcePolicy::TextOnly,
        resource_policy: Some(detached.resource_policy),
        created_at,
    };
    let json = serde_json::to_string_pretty(&rec)?;
    // Atomic write: create 0600 from birth (no world-readable window),
    // write temp in the same dir, then rename over the final path (M7).
    // NOTE: the file contains a bearer DevTools URL by design - 0600 is
    // the control; treat the cache dir as trusted (same user only).
    let tmp = dir.join(format!(".{id}.tmp"));
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(&tmp)?;
        use std::io::Write;
        f.write_all(json.as_bytes())?;
        f.sync_all()?;
    }
    #[cfg(not(unix))]
    {
        fs::write(&tmp, &json)?;
    }
    fs::rename(&tmp, dir.join(format!("{id}.json")))?;
    Ok(())
}

pub fn read_session(id: &str) -> anyhow::Result<Option<PersistedSession>> {
    let id = match sanitized(id) {
        Some(s) => s,
        None => return Ok(None),
    };
    let path = session_file(&id);
    if !path.is_file() {
        return Ok(None);
    }
    let text = fs::read_to_string(&path)?;
    match serde_json::from_str(&text) {
        Ok(rec) => Ok(Some(rec)),
        Err(e) => {
            // Corrupt file: report it, drop it, so `close` can still clean up.
            tracing::warn!(path = %path.display(), error = %e, "corrupt session file - removing");
            let _ = fs::remove_file(&path);
            Ok(None)
        }
    }
}

pub fn remove_session(id: &str) -> anyhow::Result<()> {
    if let Some(rec) = read_session(id)? {
        let _ = fs::remove_file(session_file(id));
        // Best-effort profile cleanup - ONLY under the temp dir to avoid
        // deleting a caller-supplied profile path (H1 defense in depth).
        // Canonicalize first: `starts_with` alone lets `/tmp/../etc` pass.
        if let Ok(p) = std::fs::canonicalize(&rec.user_data_dir) {
            let tmp = std::fs::canonicalize(std::env::temp_dir())
                .unwrap_or_else(|_| std::env::temp_dir());
            if p.starts_with(&tmp) && p.components().count() >= 3 && p != tmp {
                let _ = fs::remove_dir_all(&p);
            }
        }
    }
    Ok(())
}

/// Verify `pid` still belongs to a Chrome process (H2: PID-reuse safety).
/// Unix: checks `/proc/<pid>/cmdline` for "chrome" (or "chromium").
/// Windows: best-effort `true` (no /proc; `tasklist` check is brittle).
pub fn pid_belongs_to_chrome(pid: u32) -> bool {
    if pid == 0 {
        return false;
    }
    #[cfg(target_os = "linux")]
    {
        if let Ok(cmdline) = std::fs::read_to_string(format!("/proc/{pid}/cmdline")) {
            let lower = cmdline.to_ascii_lowercase();
            return lower.contains("chrome") || lower.contains("chromium");
        }
        // /proc unreadable: refuse rather than risk a wrong kill.
        false
    }
    #[cfg(all(unix, not(target_os = "linux")))]
    {
        // macOS/BSD: no /proc - ask `ps` for the command name.
        let out = std::process::Command::new("ps")
            .args(["-p", &pid.to_string(), "-o", "comm="])
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null())
            .output();
        match out {
            Ok(o) if o.status.success() => {
                let comm = String::from_utf8_lossy(&o.stdout).to_ascii_lowercase();
                comm.contains("chrome") || comm.contains("chromium")
            }
            _ => false,
        }
    }
    #[cfg(not(unix))]
    {
        true
    }
}

/// Best-effort pid liveness (Unix `/proc`; Windows assumes alive).
pub fn pid_alive(pid: u32) -> bool {
    if pid == 0 {
        return false;
    }
    #[cfg(unix)]
    {
        if std::path::Path::new(&format!("/proc/{pid}")).exists() {
            return true;
        }
        // Fallback to signal-0 probe on non-Linux Unix (macOS).
        let status = std::process::Command::new("kill")
            .args(["-0", &pid.to_string()])
            .status();
        status.map(|s| s.success()).unwrap_or(false)
    }
    #[cfg(not(unix))]
    {
        true
    }
}

/// Kill pid (`close`).
pub fn kill_pid(pid: u32) {
    #[cfg(unix)]
    {
        let _ = std::process::Command::new("kill")
            .arg(pid.to_string())
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status();
        // SIGKILL fallback (quiet - process may already be gone).
        let _ = std::process::Command::new("kill")
            .args(["-9", &pid.to_string()])
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status();
    }
    #[cfg(windows)]
    {
        let _ = std::process::Command::new("taskkill")
            .args(["/PID", &pid.to_string(), "/T", "/F"])
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status();
    }
    #[cfg(not(any(unix, windows)))]
    {
        let _ = pid;
    }
}
