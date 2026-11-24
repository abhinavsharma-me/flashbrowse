//! Platform-agnostic Chromium discovery, process spawning, stderr scraping.
//!
//! Priority order for binary resolution (locked decision):
//! 1. explicit `--chrome-path` / `LaunchOptions::chrome_path`
//! 2. `CHROME_PATH` env
//! 3. `CHROME_BINARY` env
//! 4. PATH: `google-chrome-stable`, `google-chrome`, `chromium`, `chromium-browser`
//! 5. macOS: `/Applications/Google Chrome.app/...`, `/Applications/Chromium.app/...`
//! 6. Windows: `Program Files/Google/Chrome/...`, `LocalAppData/Google/Chrome/...`

use std::collections::hash_map::DefaultHasher;
use std::env;
use std::fs::create_dir_all;
use std::hash::{Hash, Hasher};
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime};

use regex::Regex;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::{Child, Command};
use tokio::sync::oneshot;
use tokio::time::timeout;

use crate::error::CdpError;

/// Seconds to wait for `DevTools listening on ...` on stderr.
const DEVTOOLS_TIMEOUT_SECS: u64 = 20;

/// Matches Chromium stderr line, e.g.
/// `DevTools listening on ws://127.0.0.1:9222/devtools/browser/abc-123`
fn devtools_regex() -> Regex {
    // ws://127.0.0.1:<port>/devtools/browser/<uuid> (also [::1]/localhost on some builds)
    Regex::new(r"DevTools listening on (ws://[^\s]+/devtools/browser/[a-fA-F0-9-]+)")
        .expect("regex is valid")
}

/// Options for launching a headless Chromium instance.
#[derive(Debug, Clone)]
pub struct LaunchOptions {
    /// Explicit binary path (highest priority).
    pub chrome_path: Option<PathBuf>,
    /// Extra CLI args appended after the hardened defaults.
    pub extra_args: Vec<String>,
    /// If `Some(true)`, pass `--no-sandbox` (containers / CI).
    /// Defaults from `FLASHBROWSE_NO_SANDBOX` env at launch time.
    pub no_sandbox: Option<bool>,
    /// If `Some(dir)`, use as `--user-data-dir`. Otherwise a fresh temp dir.
    pub user_data_dir: Option<PathBuf>,
    /// Window size `WxH`, default `1280x800`.
    pub window_size: (u32, u32),
    /// Skip `PR_SET_PDEATHSIG` on Linux so detached persistent sessions survive CLI exit.
    pub persistent: bool,
}

impl Default for LaunchOptions {
    fn default() -> Self {
        Self {
            chrome_path: None,
            extra_args: Vec::new(),
            no_sandbox: None,
            user_data_dir: None,
            window_size: (1280, 800),
            persistent: false,
        }
    }
}

/// Pipe file descriptors for `--remote-debugging-pipe` on Unix (parent read = FD 4, parent write = FD 3).
#[cfg(unix)]
pub struct PipeHandles {
    pub read_fd: std::os::unix::io::OwnedFd,
    pub write_fd: std::os::unix::io::OwnedFd,
}

/// Handle to a running Chromium process.
///
/// On drop, the child is killed and reaped on a best-effort basis so CLI exit
/// never leaks a browser. Callers needing persistence (session files) should
/// detach ownership explicitly (e.g. keep pid + ws_url, then drop the guard
/// after killing a clone - no, use `detach`).
pub struct ChromeProcess {
    child: Option<Child>,
    browser_ws_url: String,
    chrome_path: PathBuf,
    user_data_dir: PathBuf,
    /// `true` only for auto-created temp dirs - caller-supplied
    /// `--user-data-dir` is never deleted on drop (H1 fix).
    owned_dir: bool,
    #[cfg(unix)]
    pipe_handles: Option<PipeHandles>,
}

impl ChromeProcess {
    /// Browser-level WebSocket URL: `ws://127.0.0.1:.../devtools/browser/...`
    pub fn browser_ws_url(&self) -> &str {
        &self.browser_ws_url
    }

    /// Resolved Chromium binary path.
    pub fn chrome_path(&self) -> &PathBuf {
        &self.chrome_path
    }

    /// User-data dir used for this instance.
    pub fn user_data_dir(&self) -> &PathBuf {
        &self.user_data_dir
    }

    /// OS process id.
    pub fn pid(&self) -> Option<u32> {
        self.child.as_ref().and_then(|c| c.id())
    }

    /// Take the `--remote-debugging-pipe` descriptors if launched in pipe mode.
    #[cfg(unix)]
    pub fn take_pipe_handles(&mut self) -> Option<PipeHandles> {
        self.pipe_handles.take()
    }

    /// Kill the whole process tree (Unix: process group) and wait.
    /// Removes the auto-created profile dir (owned dirs only).
    pub async fn shutdown(&mut self) {
        self.kill_tree().await;
        if self.owned_dir {
            let _ = std::fs::remove_dir_all(&self.user_data_dir);
        }
    }

    async fn kill_tree(&mut self) {
        #[cfg(unix)]
        if let Some(pid) = self.pid() {
            // Kill the process group (-pid) first so zygote/GPU children die.
            let _ = std::process::Command::new("kill")
                .arg(format!("-{}", pid))
                .arg("-9")
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status();
        }
        if let Some(mut child) = self.child.take() {
            let _ = child.kill().await;
            let _ = child.wait().await;
        }
    }

    /// Detach: release the child process (it keeps running), suppressing
    /// the `Drop` kill. Returns the raw child handle plus the profile dir,
    /// which the caller now owns (session-file owner removes on `close`).
    ///
    /// Returns `None` if the process was already shut down.
    pub fn detach(mut self) -> Option<(Child, PathBuf)> {
        let child = self.child.take()?;
        let dir = std::mem::take(&mut self.user_data_dir);
        std::mem::forget(self);
        Some((child, dir))
    }
}

impl Drop for ChromeProcess {
    fn drop(&mut self) {
        if let Some(child) = self.child.take() {
            // tokio::Child::kill_on_drop + start_kill handles teardown.
            // tokio::process::Child::start_kill() is sync; reaping is
            // best-effort (OS reaps on process exit).
            let mut child = child;
            let _ = child.start_kill();
            drop(child);
        }
        if self.owned_dir {
            // Synchronous removal: a detached thread was killed by process
            // exit before finishing, leaking the profile dir.
            let _ = std::fs::remove_dir_all(&self.user_data_dir);
        }
    }
}

/// Resolve a Chromium binary using the locked priority order.
pub fn find_chrome(explicit: Option<&PathBuf>) -> Result<PathBuf, CdpError> {
    let mut searched: Vec<String> = Vec::new();

    if let Some(p) = explicit {
        searched.push(format!("explicit: {}", p.display()));
        if p.is_file() {
            return Ok(p.clone());
        }
        return Err(CdpError::NoChromeBinary { searched });
    }

    for env_key in ["CHROME_PATH", "CHROME_BINARY"] {
        if let Ok(p) = env::var(env_key) {
            searched.push(format!("${env_key}: {p}"));
            let pb = PathBuf::from(&p);
            if pb.is_file() {
                return Ok(pb);
            }
        }
    }

    let prefer_managed = std::env::var("FLASHBROWSE_PREFER_MANAGED")
        .map(|v| {
            matches!(
                v.trim().to_ascii_lowercase().as_str(),
                "1" | "true" | "yes" | "on"
            )
        })
        .unwrap_or(false);
    if prefer_managed {
        for cand in managed_chrome_paths() {
            searched.push(format!("managed: {}", cand.display()));
            if cand.is_file() {
                return Ok(cand);
            }
        }
    }

    for name in [
        "google-chrome-stable",
        "google-chrome",
        "chromium",
        "chromium-browser",
    ] {
        searched.push(format!("PATH:{name}"));
        if let Ok(p) = which::which(name) {
            if p.is_file() {
                return Ok(p);
            }
        }
    }

    #[cfg(target_os = "macos")]
    for cand in [
        "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome",
        "/Applications/Chromium.app/Contents/MacOS/Chromium",
    ] {
        searched.push(cand.to_string());
        let pb = PathBuf::from(cand);
        if pb.is_file() {
            return Ok(pb);
        }
    }

    // Managed download (`flashbrowse setup`) - searched before hardcoded
    // system paths so a first-run install just works.
    for cand in managed_chrome_paths() {
        searched.push(format!("managed: {}", cand.display()));
        if cand.is_file() {
            return Ok(cand);
        }
    }

    #[cfg(target_os = "windows")]
    for cand in windows_chrome_paths() {
        searched.push(cand.display().to_string());
        if cand.is_file() {
            return Ok(cand);
        }
    }

    Err(CdpError::NoChromeBinary { searched })
}

/// Managed Chrome paths (installed by `flashbrowse setup`).
fn managed_chrome_paths() -> Vec<PathBuf> {
    let root = managed_chrome_root();
    let mut out = vec![
        root.join("chrome-headless-shell"),
        root.join("chrome-headless-shell.exe"),
        root.join("chrome"),
        root.join("chrome.exe"),
    ];
    if let Ok(rd) = std::fs::read_dir(&root) {
        for entry in rd.flatten() {
            out.push(entry.path().join("chrome-headless-shell"));
            out.push(entry.path().join("chrome-headless-shell.exe"));
            out.push(entry.path().join("chrome"));
            out.push(entry.path().join("chrome.exe"));
        }
    }
    out
}

fn managed_chrome_root() -> PathBuf {
    #[cfg(windows)]
    {
        if let Ok(local) = env::var("LOCALAPPDATA") {
            return PathBuf::from(local).join("flashbrowse").join("chrome");
        }
    }
    if let Ok(xdg) = env::var("XDG_CACHE_HOME") {
        if !xdg.is_empty() {
            return PathBuf::from(xdg).join("flashbrowse").join("chrome");
        }
    }
    let home = env::var("HOME").unwrap_or_else(|_| "/tmp".to_string());
    PathBuf::from(home)
        .join(".cache")
        .join("flashbrowse")
        .join("chrome")
}

#[cfg(target_os = "windows")]
fn windows_chrome_paths() -> Vec<PathBuf> {
    let mut out = vec![
        PathBuf::from(r"C:\Program Files\Google\Chrome\Application\chrome.exe"),
        PathBuf::from(r"C:\Program Files (x86)\Google\Chrome\Application\chrome.exe"),
    ];
    if let Ok(local) = env::var("LOCALAPPDATA") {
        out.push(PathBuf::from(local).join(r"Google\Chrome\Application\chrome.exe"));
    }
    if let Ok(program_files) = env::var("ProgramFiles") {
        out.push(PathBuf::from(program_files).join(r"Google\Chrome\Application\chrome.exe"));
    }
    out
}

/// Hardened, leak-proof Chromium flags (spec + `--user-data-dir`, `--remote-debugging-pipe` / `--remote-debugging-port=0`).
fn hardened_args(
    user_data_dir: &std::path::Path,
    window_size: (u32, u32),
    no_sandbox: bool,
    use_pipe: bool,
    also_port: bool,
) -> Vec<String> {
    let mut args = vec![
        "--headless=new".to_string(),
        "--disable-gpu".to_string(),
        "--disable-dev-shm-usage".to_string(),
        "--no-first-run".to_string(),
        "--no-default-browser-check".to_string(),
        "--disable-background-networking".to_string(),
        "--disable-background-timer-throttling".to_string(),
        "--disable-client-side-phishing-detection".to_string(),
        "--disable-default-apps".to_string(),
        "--disable-extensions".to_string(),
        "--disable-sync".to_string(),
        "--mute-audio".to_string(),
        format!("--window-size={},{}", window_size.0, window_size.1),
        format!("--user-data-dir={}", user_data_dir.display()),
    ];
    if use_pipe {
        args.push("--remote-debugging-pipe".to_string());
        if also_port {
            args.push("--remote-debugging-port=0".to_string());
        }
    } else {
        args.push("--remote-debugging-port=0".to_string());
    }
    if no_sandbox {
        args.push("--no-sandbox".to_string());
    }
    args
}

static PROFILE_SEQ: AtomicU64 = AtomicU64::new(0);

fn fresh_user_data_dir() -> std::io::Result<PathBuf> {
    // Secure temp dir: 0700, exclusive-create, symlink-rejecting (H3 fix).
    // Falls back to hash naming only for the integer suffix; creation is
    // still O_EXCL via create_new semantics on the DirBuilder mode.
    let seq = PROFILE_SEQ.fetch_add(1, Ordering::Relaxed);
    let mut hasher = DefaultHasher::new();
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0)
        .hash(&mut hasher);
    std::process::id().hash(&mut hasher);
    seq.hash(&mut hasher);
    let hash = hasher.finish();
    let base = env::temp_dir().join(format!("flashbrowse-{}-{:x}", std::process::id(), hash));
    // Try base, then base-N suffixes on collision/symlink.
    for attempt in 0..10u32 {
        let dir = if attempt == 0 {
            base.clone()
        } else {
            env::temp_dir().join(format!("{}-{}", base.display(), attempt))
        };
        // Reject pre-existing symlinks (TOCTOU mitigation).
        if let Ok(md) = std::fs::symlink_metadata(&dir) {
            if md.file_type().is_symlink() || md.is_dir() {
                let _ = std::fs::remove_dir_all(&dir);
                // If it was a real dir from another run, keep probing suffixes.
                continue;
            }
        }
        let mut builder = std::fs::DirBuilder::new();
        builder.recursive(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::DirBuilderExt;
            builder.mode(0o700);
        }
        match builder.create(&dir) {
            Ok(()) => {
                #[cfg(unix)]
                {
                    use std::os::unix::fs::PermissionsExt;
                    let _ = std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700));
                }
                return Ok(dir);
            }
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(e),
        }
    }
    // Last resort: create_dir_all (better than failing the launch).
    create_dir_all(&base)?;
    Ok(base)
}

fn resolve_no_sandbox(opt: Option<bool>) -> bool {
    opt.unwrap_or_else(|| {
        env::var("FLASHBROWSE_NO_SANDBOX")
            .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
            .unwrap_or(false)
    })
}

/// Remove leftover profile dirs from crashed/killed runs.
///
/// Best-effort: only dirs that look like ours (`flashbrowse-<pid>-<hash>`),
/// whose pid is gone and that are older than `STALE_PROFILE_SECS`.
fn cleanup_stale_profiles() {
    const STALE_PROFILE_SECS: u64 = 3600;
    let tmp = env::temp_dir();
    let Ok(rd) = std::fs::read_dir(&tmp) else {
        return;
    };
    let now = SystemTime::now();
    for entry in rd.flatten() {
        let name = entry.file_name();
        let Some(name) = name.to_str() else { continue };
        if !name.starts_with("flashbrowse-") {
            continue;
        }
        // flashbrowse-<pid>-<hash>
        let mut parts = name.split('-');
        let _ = parts.next();
        let Some(pid) = parts.next().and_then(|p| p.parse::<u32>().ok()) else {
            continue;
        };
        let path = entry.path();
        // Only remove if the owning process is gone and the dir is old.
        if std::path::Path::new(&format!("/proc/{pid}")).exists() {
            continue;
        }
        let Ok(meta) = entry.metadata() else { continue };
        let Ok(modified) = meta.modified() else {
            continue;
        };
        let Ok(age) = now.duration_since(modified) else {
            continue;
        };
        if age.as_secs() > STALE_PROFILE_SECS {
            let _ = std::fs::remove_dir_all(path);
        }
    }
}

/// Launch headless Chromium.
///
/// On Unix, ephemeral/MCP launches (`!opts.persistent`) use
/// `--remote-debugging-pipe` (falling back to WebSocket if unsupported).
/// Persistent launches (`opts.persistent`) use WebSocket so Chromium stays
/// alive after the spawning CLI process exits and closes its file descriptors.
pub async fn launch_chrome(opts: LaunchOptions) -> Result<ChromeProcess, CdpError> {
    cleanup_stale_profiles();
    #[cfg(unix)]
    {
        if !opts.persistent
            && !opts
                .extra_args
                .iter()
                .any(|a| a.starts_with("--remote-debugging-port"))
        {
            match launch_chrome_pipe(&opts).await {
                Ok(proc) => return Ok(proc),
                Err(err) => {
                    tracing::warn!(%err, "pipe launch failed; falling back to WebSocket");
                }
            }
        }
    }
    launch_chrome_ws(opts).await
}

#[cfg(unix)]
async fn launch_chrome_pipe(opts: &LaunchOptions) -> Result<ChromeProcess, CdpError> {
    use std::os::unix::io::{FromRawFd, OwnedFd, RawFd};

    let chrome_path = find_chrome(opts.chrome_path.as_ref())?;
    let (user_data_dir, owned_dir) = match &opts.user_data_dir {
        Some(d) => {
            create_dir_all(d)?;
            (d.clone(), false)
        }
        None => (fresh_user_data_dir()?, true),
    };
    let no_sandbox = resolve_no_sandbox(opts.no_sandbox);
    let args = hardened_args(
        &user_data_dir,
        opts.window_size,
        no_sandbox,
        true,
        opts.persistent,
    );

    let mut to_chrome: [RawFd; 2] = [0; 2];
    let mut from_chrome: [RawFd; 2] = [0; 2];
    unsafe {
        if libc::pipe(to_chrome.as_mut_ptr()) != 0 {
            return Err(CdpError::SpawnFailed {
                path: chrome_path,
                source: std::io::Error::last_os_error(),
            });
        }
        if libc::pipe(from_chrome.as_mut_ptr()) != 0 {
            let err = std::io::Error::last_os_error();
            libc::close(to_chrome[0]);
            libc::close(to_chrome[1]);
            return Err(CdpError::SpawnFailed {
                path: chrome_path,
                source: err,
            });
        }
    }
    let child_read_fd = to_chrome[0];
    let parent_write_fd = to_chrome[1];
    let parent_read_fd = from_chrome[0];
    let child_write_fd = from_chrome[1];

    let mut command = Command::new(&chrome_path);
    command
        .args(&args)
        .args(&opts.extra_args)
        .stdout(Stdio::null())
        .stderr(if opts.persistent {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdin(Stdio::null())
        .kill_on_drop(true);

    let persistent = opts.persistent;
    unsafe {
        command.pre_exec(move || {
            if libc::setpgid(0, 0) != 0 {
                return Err(std::io::Error::last_os_error());
            }
            #[cfg(target_os = "linux")]
            if !persistent && libc::prctl(1, libc::SIGKILL as libc::c_int, 0, 0, 0) != 0 {
                return Err(std::io::Error::last_os_error());
            }
            if libc::dup2(child_read_fd, 3) < 0 {
                return Err(std::io::Error::last_os_error());
            }
            if libc::dup2(child_write_fd, 4) < 0 {
                return Err(std::io::Error::last_os_error());
            }
            if child_read_fd > 4 {
                libc::close(child_read_fd);
            }
            if child_write_fd > 4 {
                libc::close(child_write_fd);
            }
            libc::close(parent_write_fd);
            libc::close(parent_read_fd);
            Ok(())
        });
    }

    let spawn_res = command.spawn();
    unsafe {
        libc::close(child_read_fd);
        libc::close(child_write_fd);
    }
    let mut child = match spawn_res {
        Ok(c) => c,
        Err(source) => {
            unsafe {
                libc::close(parent_write_fd);
                libc::close(parent_read_fd);
            }
            if owned_dir {
                let _ = std::fs::remove_dir_all(&user_data_dir);
            }
            return Err(CdpError::SpawnFailed {
                path: chrome_path,
                source,
            });
        }
    };

    let read_fd = unsafe { OwnedFd::from_raw_fd(parent_read_fd) };
    let write_fd = unsafe { OwnedFd::from_raw_fd(parent_write_fd) };

    let browser_ws_url = if opts.persistent {
        await_devtools_ws_url(&mut child).await?
    } else {
        String::new()
    };

    tracing::debug!(
        chrome = %chrome_path.display(),
        persistent = opts.persistent,
        "Chromium launched in pipe mode"
    );

    Ok(ChromeProcess {
        child: Some(child),
        browser_ws_url,
        chrome_path,
        user_data_dir,
        owned_dir,
        pipe_handles: Some(PipeHandles { read_fd, write_fd }),
    })
}

async fn await_devtools_ws_url(child: &mut Child) -> Result<String, CdpError> {
    let stderr = child.stderr.take().expect("stderr piped");
    let re = devtools_regex();
    let (tx, rx) = oneshot::channel::<String>();

    tokio::spawn(async move {
        let mut reader = BufReader::new(stderr);
        let mut line = String::new();
        loop {
            line.clear();
            match reader.read_line(&mut line).await {
                Ok(0) => break,
                Ok(_) => {
                    if let Some(caps) = re.captures(&line) {
                        if let Some(m) = caps.get(1) {
                            let _ = tx.send(m.as_str().to_string());
                            break;
                        }
                    }
                }
                Err(_) => break,
            }
        }
    });

    match timeout(Duration::from_secs(DEVTOOLS_TIMEOUT_SECS), rx).await {
        Ok(Ok(url)) => Ok(url),
        Ok(Err(_)) => {
            let code = child.try_wait().ok().flatten().and_then(|s| s.code());
            let _ = child.kill().await;
            Err(CdpError::ChromiumExitedEarly { code })
        }
        Err(_) => {
            let _ = child.kill().await;
            Err(CdpError::DevToolsTimeout {
                seconds: DEVTOOLS_TIMEOUT_SECS,
            })
        }
    }
}

/// Launch headless Chromium over WebSocket (`--remote-debugging-port=0`).
pub async fn launch_chrome_ws(opts: LaunchOptions) -> Result<ChromeProcess, CdpError> {
    let chrome_path = find_chrome(opts.chrome_path.as_ref())?;
    let (user_data_dir, owned_dir) = match opts.user_data_dir {
        Some(d) => {
            create_dir_all(&d)?;
            (d, false)
        }
        None => (fresh_user_data_dir()?, true),
    };
    let no_sandbox = resolve_no_sandbox(opts.no_sandbox);
    let args = hardened_args(&user_data_dir, opts.window_size, no_sandbox, false, false);

    let mut command = Command::new(&chrome_path);
    command
        .args(&args)
        .args(&opts.extra_args)
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .stdin(Stdio::null())
        .kill_on_drop(true);
    #[cfg(unix)]
    unsafe {
        command.pre_exec(|| {
            if libc::setpgid(0, 0) == 0 {
                Ok(())
            } else {
                Err(std::io::Error::last_os_error())
            }
        });
    }
    #[cfg(target_os = "linux")]
    if !opts.persistent {
        unsafe {
            command.pre_exec(|| {
                if libc::prctl(1, libc::SIGKILL as libc::c_int, 0, 0, 0) != 0 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
    }
    #[cfg(windows)]
    {
        clear_stdio_inherit_flags();
        if opts.persistent {
            const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
            command.creation_flags(CREATE_NEW_PROCESS_GROUP);
        }
    }

    let mut child = command.spawn().map_err(|source| CdpError::SpawnFailed {
        path: chrome_path.clone(),
        source,
    })?;

    let browser_ws_url = await_devtools_ws_url(&mut child).await?;

    tracing::debug!(
        chrome = %chrome_path.display(),
        ws = %browser_ws_url,
        "Chromium launched, DevTools listening"
    );

    Ok(ChromeProcess {
        child: Some(child),
        browser_ws_url,
        chrome_path,
        user_data_dir,
        owned_dir,
        #[cfg(unix)]
        pipe_handles: None,
    })
}

#[cfg(windows)]
fn clear_stdio_inherit_flags() {
    const STD_INPUT_HANDLE: u32 = 0xFFFF_FFF6;
    const STD_OUTPUT_HANDLE: u32 = 0xFFFF_FFF5;
    const STD_ERROR_HANDLE: u32 = 0xFFFF_FFF4;
    const HANDLE_FLAG_INHERIT: u32 = 0x0000_0001;
    const INVALID_HANDLE_VALUE: isize = -1;

    extern "system" {
        fn GetStdHandle(n_std_handle: u32) -> isize;
        fn SetHandleInformation(h_object: isize, dw_mask: u32, dw_flags: u32) -> i32;
    }

    for std_handle in [STD_INPUT_HANDLE, STD_OUTPUT_HANDLE, STD_ERROR_HANDLE] {
        unsafe {
            let h = GetStdHandle(std_handle);
            if h != 0 && h != INVALID_HANDLE_VALUE {
                let _ = SetHandleInformation(h, HANDLE_FLAG_INHERIT, 0);
            }
        }
    }
}
