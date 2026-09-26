//! Session-scoped sandbox path-mapping **state** — data/state-only (no UI
//! code; the `/sandbox` menu nodes live in `v25_sandbox`).
//!
//! Stores the session bindings, a snapshot of the **base** config (from
//! `settings.toml`), and the exclusion set. Session entries are merged
//! into the live `SandboxConfig` before each `bwrap` invocation
//! ([`merge_into`]) but are **never** persisted. The base snapshot lets
//! listings show config-provided bindings and lets the user temporarily
//! exclude them (re-enabled via [`restore`]).
//!
//! All public functions are thread-safe (backed by a `Mutex`).

use std::path::PathBuf;
use std::sync::{LazyLock, Mutex};

use crate::config::{BindMode, SandboxConfig};
use crate::path_util;

/// A single temporary binding added during the session.
///
/// A simple bind has `sandbox_path == host_path`; a remap has them different.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionBind {
    pub host_path: PathBuf,
    /// In-sandbox destination path.  Equal to `host_path` for a simple bind.
    pub sandbox_path: PathBuf,
    pub mode: BindMode,
}

/// A binding as shown in the combined overlay (config + session).
#[derive(Debug, Clone)]
pub struct CombinedBind {
    pub host_path: PathBuf,
    pub sandbox_path: PathBuf,
    pub mode: BindMode,
    /// `true` for ad-hoc session binds; `false` for config-provided binds.
    pub is_session: bool,
    /// `true` when a config-provided binding has been excluded by the user.
    pub is_excluded: bool,
}

/// Global session-scoped additional bindings.
static SESSION_BINDS: LazyLock<Mutex<Vec<SessionBind>>> = LazyLock::new(|| Mutex::new(Vec::new()));

/// Snapshot of the *original* sandbox config (before session merge), captured
/// at startup so we can distinguish config-provided bindings from ad-hoc ones.
static BASE_CONFIG: LazyLock<Mutex<Option<SandboxConfig>>> = LazyLock::new(|| Mutex::new(None));

/// Sandbox paths (resolved, in-sandbox) that the user has temporarily excluded
/// from the config.  These are skipped during [`merge_into`].
static EXCLUDED: LazyLock<Mutex<Vec<PathBuf>>> = LazyLock::new(|| Mutex::new(Vec::new()));

/// Store the original (non-merged) sandbox config so we can list config-
/// provided bindings in the remove overlay and rebuild correctly on merge.
pub fn set_base(cfg: Option<SandboxConfig>) {
    if let Ok(mut b) = BASE_CONFIG.lock() {
        *b = cfg;
    }
}

/// Mark a config-provided sandbox path as excluded (temporarily removed).
pub fn exclude(sandbox_path: &str) -> Result<(), String> {
    let resolved = resolve_sandbox_path(sandbox_path);
    let mut exc = EXCLUDED.lock().map_err(|e| format!("lock error: {}", e))?;
    if !exc.contains(&resolved) {
        exc.push(resolved);
    }
    Ok(())
}

/// Re-enable a temporarily excluded config binding (drop it from the
/// exclusion list).
pub fn restore(sandbox_path: &str) -> Result<(), String> {
    let resolved = resolve_sandbox_path(sandbox_path);
    let mut exc = EXCLUDED.lock().map_err(|e| format!("lock error: {}", e))?;
    exc.retain(|p| *p != resolved);
    Ok(())
}

/// Return a copy of the currently excluded sandbox paths.
pub fn list_excluded() -> Vec<PathBuf> {
    EXCLUDED.lock().map(|e| e.clone()).unwrap_or_default()
}

/// Add (or upgrade) a session binding with the given `mode`.
///
/// `host_path` is resolved against the CWD (`~` expanded); it must exist.
/// `sandbox_path` is the in-sandbox destination (defaults to `host_path` when
/// empty).  An existing binding with the same sandbox path is upgraded
/// (mode + host overwritten).  Returns an error if the host path does not
/// exist.
pub fn add(host_path: &str, sandbox_path: &str, mode: BindMode) -> Result<(), String> {
    let host = path_util::resolve_canonical(host_path)?;
    let sandbox = if sandbox_path.is_empty() {
        host.clone()
    } else {
        resolve_sandbox_path(sandbox_path)
    };
    let mut binds = SESSION_BINDS
        .lock()
        .map_err(|e| format!("lock error: {}", e))?;
    // Dedup by sandbox (in-sandbox) path.
    if let Some(pos) = binds.iter().position(|b| b.sandbox_path == sandbox) {
        binds[pos].mode = mode;
        binds[pos].host_path = host;
    } else {
        binds.push(SessionBind {
            host_path: host,
            sandbox_path: sandbox,
            mode,
        });
    }
    Ok(())
}

/// Read-only shorthand for [`add`] (kept for the v2.0 overlay handler,
/// retired with the v2.5 `/sandbox` port).
pub fn add_ro(host_path: &str, sandbox_path: &str) -> Result<(), String> {
    add(host_path, sandbox_path, BindMode::Ro)
}

/// Read-write shorthand for [`add`] (kept for the v2.0 overlay handler,
/// retired with the v2.5 `/sandbox` port).
pub fn add_rw(host_path: &str, sandbox_path: &str) -> Result<(), String> {
    add(host_path, sandbox_path, BindMode::Rw)
}

/// Remove a path binding from the session.
///
/// `sandbox_path` is the in-sandbox path to remove (the "to" component).
/// Returns an error if no matching binding is found.
pub fn remove(sandbox_path: &str) -> Result<(), String> {
    let target = resolve_sandbox_path(sandbox_path);
    let mut binds = SESSION_BINDS
        .lock()
        .map_err(|e| format!("lock error: {}", e))?;
    let pos = binds
        .iter()
        .position(|b| b.sandbox_path == target)
        .ok_or_else(|| format!("no session binding for sandbox path: {}", target.display()))?;
    binds.remove(pos);
    Ok(())
}

/// Return a copy of the current session bindings.
pub fn list() -> Vec<SessionBind> {
    SESSION_BINDS.lock().map(|b| b.clone()).unwrap_or_default()
}

/// Return the combined list of all visible bindings: config-provided (minus
/// excluded) plus session-provided.  The result distinguishes origin so the
/// overlay can display and fuzzy-match the full picture.
pub fn combined_list() -> Vec<CombinedBind> {
    let base = BASE_CONFIG.lock().ok().and_then(|b| b.clone());
    let excluded = EXCLUDED.lock().map(|e| e.clone()).unwrap_or_default();
    let binds = list();
    let cwd = std::env::current_dir().ok();
    let home = std::env::var("HOME").ok().map(PathBuf::from);
    let mut out: Vec<CombinedBind> = Vec::new();

    // Resolve a single BindPath (config entry) into a CombinedBind.
    let resolve_cfg = |spec: &crate::config::BindPath, mode: BindMode| -> CombinedBind {
        let (from, to) = match spec {
            crate::config::BindPath::Simple(p) => {
                let r = path_util::resolve_with_home(
                    p,
                    cwd.as_deref().unwrap_or(std::path::Path::new(".")),
                    home.as_deref(),
                );
                (r.clone(), r)
            }
            crate::config::BindPath::Remap { from, to } => {
                let f = path_util::resolve_with_home(
                    from,
                    cwd.as_deref().unwrap_or(std::path::Path::new(".")),
                    home.as_deref(),
                );
                let t = path_util::resolve_with_home(
                    to,
                    cwd.as_deref().unwrap_or(std::path::Path::new(".")),
                    home.as_deref(),
                );
                (f, t)
            }
        };
        CombinedBind {
            host_path: from,
            sandbox_path: to.clone(),
            mode,
            is_session: false,
            is_excluded: excluded.contains(&to),
        }
    };

    if let Some(ref cfg) = base {
        for spec in &cfg.ro {
            out.push(resolve_cfg(spec, BindMode::Ro));
        }
        for spec in &cfg.rw {
            out.push(resolve_cfg(spec, BindMode::Rw));
        }
    }

    // Session bindings: track which sandbox paths have a session override so we
    // can suppress the corresponding config entry (session wins on conflict).
    let session_paths: Vec<PathBuf> = binds.iter().map(|b| b.sandbox_path.clone()).collect();

    // Remove config entries overridden by session bindings.
    out.retain(|cb| {
        if cb.is_session {
            return true;
        }
        !session_paths.contains(&cb.sandbox_path)
    });

    for b in &binds {
        out.push(CombinedBind {
            host_path: b.host_path.clone(),
            sandbox_path: b.sandbox_path.clone(),
            mode: b.mode,
            is_session: true,
            is_excluded: false,
        });
    }

    out
}

/// Merge session bindings into a `SandboxConfig` **clone**.
///
/// Rebuilds the config's `ro`/`rw` lists from scratch: starts with the base
/// config (minus any excluded paths), then appends session bindings (which
/// override matching config entries via bwrap's "last wins" semantics).
/// Callers should clone their config first and pass the clone here.
pub fn merge_into(cfg: &mut SandboxConfig) {
    let base = BASE_CONFIG.lock().ok().and_then(|b| b.clone());
    let excluded = EXCLUDED.lock().map(|e| e.clone()).unwrap_or_default();
    let binds = list();

    // Rebuild from base config, skipping excluded paths.
    cfg.ro.clear();
    cfg.rw.clear();

    if let Some(ref base_cfg) = base {
        for spec in &base_cfg.ro {
            let to = sandbox_path_of(spec);
            if !excluded.contains(&to) {
                cfg.ro.push(spec.clone());
            }
        }
        for spec in &base_cfg.rw {
            let to = sandbox_path_of(spec);
            if !excluded.contains(&to) {
                cfg.rw.push(spec.clone());
            }
        }
    }

    // Append session bindings.
    for b in &binds {
        let from = b.host_path.to_string_lossy().to_string();
        let to = b.sandbox_path.to_string_lossy().to_string();
        let bind = if from == to {
            crate::config::BindPath::Simple(from)
        } else {
            crate::config::BindPath::Remap {
                from: from.clone(),
                to: to.clone(),
            }
        };
        match b.mode {
            BindMode::Ro => cfg.ro.push(bind),
            BindMode::Rw => cfg.rw.push(bind),
        }
    }
}

/// Extract the in-sandbox destination path from a `BindPath`.
fn sandbox_path_of(spec: &crate::config::BindPath) -> PathBuf {
    match spec {
        crate::config::BindPath::Simple(p) => {
            // Resolve tilde/relative so we can compare with excluded paths.
            let cwd = std::env::current_dir().ok();
            let home = std::env::var("HOME").ok().map(PathBuf::from);
            path_util::resolve_with_home(
                p,
                cwd.as_deref().unwrap_or(std::path::Path::new(".")),
                home.as_deref(),
            )
        }
        crate::config::BindPath::Remap { to, .. } => {
            let cwd = std::env::current_dir().ok();
            let home = std::env::var("HOME").ok().map(PathBuf::from);
            path_util::resolve_with_home(
                to,
                cwd.as_deref().unwrap_or(std::path::Path::new(".")),
                home.as_deref(),
            )
        }
    }
}

/// Helper: resolve a host path for display/validation.  Returns `None` if the
/// path does not exist.
pub fn resolve_for_display(path: &str) -> Option<PathBuf> {
    path_util::resolve_canonical(path).ok()
}

/// Resolve a sandbox path (in-sandbox destination) — tilde expansion +
/// normalization.  No filesystem access.
fn resolve_sandbox_path(path: &str) -> PathBuf {
    path_util::normalize(&path_util::expand_tilde(path))
}

/// Clear all session bindings, exclusions, and the base-config
/// snapshot (used in tests).
#[cfg(test)]
pub fn clear() {
    if let Ok(mut b) = SESSION_BINDS.lock() {
        b.clear();
    }
    if let Ok(mut e) = EXCLUDED.lock() {
        e.clear();
    }
    if let Ok(mut b) = BASE_CONFIG.lock() {
        *b = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serial_test::serial;
    use std::fs;

    fn setup() {
        clear();
    }

    #[test]
    #[serial]
    fn test_add_ro_simple() {
        setup();
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test_ro");
        fs::create_dir_all(&path).unwrap();
        let ps = path.to_string_lossy();

        add_ro(&ps, "").unwrap();
        let binds = list();
        assert_eq!(binds.len(), 1);
        assert_eq!(binds[0].mode, BindMode::Ro);
        assert_eq!(binds[0].host_path, binds[0].sandbox_path);

        // Dedup (same sandbox path)
        add_ro(&ps, "").unwrap();
        assert_eq!(list().len(), 1);
    }

    #[test]
    #[serial]
    fn test_add_ro_remap() {
        setup();
        let dir = tempfile::tempdir().unwrap();
        let host = dir.path().join("host_ro");
        fs::create_dir_all(&host).unwrap();
        let sandbox = dir.path().join("sandbox_ro");

        let hs = host.to_string_lossy();
        let ss = sandbox.to_string_lossy();
        add_ro(&hs, &ss).unwrap();
        let binds = list();
        assert_eq!(binds.len(), 1);
        assert_eq!(binds[0].mode, BindMode::Ro);
        assert_eq!(binds[0].host_path, host);
        // sandbox_path is resolved to an absolute path
        assert!(binds[0]
            .sandbox_path
            .to_string_lossy()
            .ends_with("sandbox_ro"));
        assert_ne!(binds[0].host_path, binds[0].sandbox_path);
    }

    #[test]
    #[serial]
    fn test_add_rw_simple() {
        setup();
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test_rw");
        fs::create_dir_all(&path).unwrap();
        let ps = path.to_string_lossy();

        add_rw(&ps, "").unwrap();
        let binds = list();
        assert_eq!(binds.len(), 1);
        assert_eq!(binds[0].mode, BindMode::Rw);
    }

    #[test]
    #[serial]
    fn test_dedup_upgrades_mode() {
        setup();
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("dedup");
        fs::create_dir_all(&path).unwrap();
        let ps = path.to_string_lossy();

        add_ro(&ps, "").unwrap();
        // Upgrade to rw
        add_rw(&ps, "").unwrap();
        let binds = list();
        assert_eq!(binds.len(), 1);
        assert_eq!(binds[0].mode, BindMode::Rw);
    }

    #[test]
    #[serial]
    fn test_remove_by_sandbox_path() {
        setup();
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("remove_me");
        fs::create_dir_all(&path).unwrap();
        let ps = path.to_string_lossy();

        add_ro(&ps, "").unwrap();
        assert_eq!(list().len(), 1);
        // Remove by sandbox path (same as host path for simple bind).
        remove(&ps).unwrap();
        assert_eq!(list().len(), 0);
    }

    #[test]
    #[serial]
    fn test_remove_remap() {
        setup();
        let dir = tempfile::tempdir().unwrap();
        let host = dir.path().join("host_path");
        let sandbox = dir.path().join("sandbox_path");
        fs::create_dir_all(&host).unwrap();

        let hs = host.to_string_lossy();
        let ss = sandbox.to_string_lossy();
        add_ro(&hs, &ss).unwrap();
        assert_eq!(list().len(), 1);
        // Remove by sandbox path (the "to" component).
        remove(&ss).unwrap();
        assert_eq!(list().len(), 0);
    }

    #[test]
    #[serial]
    fn test_remove_nonexistent_binding() {
        setup();
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nope");
        let ps = path.to_string_lossy();
        let result = remove(&ps);
        assert!(result.is_err());
    }

    #[test]
    #[serial]
    fn test_add_with_explicit_mode() {
        setup();
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("add_mode");
        fs::create_dir_all(&path).unwrap();
        let ps = path.to_string_lossy();

        add(&ps, "", BindMode::Rw).unwrap();
        let binds = list();
        assert_eq!(binds.len(), 1);
        assert_eq!(binds[0].mode, BindMode::Rw);
        assert_eq!(binds[0].host_path, binds[0].sandbox_path);
    }

    #[test]
    #[serial]
    fn test_restore_re_enables_excluded() {
        setup();
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("restore_me");
        fs::create_dir_all(&path).unwrap();
        let ps = path.to_string_lossy();

        exclude(&ps).unwrap();
        assert!(
            list_excluded().iter().any(|p| *p == path),
            "must be excluded after exclude()"
        );
        restore(&ps).unwrap();
        assert!(
            !list_excluded().iter().any(|p| *p == path),
            "must be re-enabled after restore()"
        );
    }
}
