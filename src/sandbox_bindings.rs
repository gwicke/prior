//! Session-scoped temporary path bindings for the sandbox.
//!
//! The `/sandbox` slash menu adds/removes entries here; they are merged into
//! the live `SandboxConfig` before each `bwrap` invocation, but are **never**
//! persisted to `settings.toml`.
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

/// Global session-scoped additional bindings.
static SESSION_BINDS: LazyLock<Mutex<Vec<SessionBind>>> =
    LazyLock::new(|| Mutex::new(Vec::new()));

/// Add a read-only path binding for the session.
///
/// `host_path` is resolved against the CWD (`~` expanded); it must exist.
/// `sandbox_path` is the in-sandbox destination (defaults to `host_path` when
/// empty).  Returns an error if the host path does not exist.
pub fn add_ro(host_path: &str, sandbox_path: &str) -> Result<(), String> {
    let host = path_util::resolve_canonical(host_path)?;
    let sandbox = if sandbox_path.is_empty() {
        host.clone()
    } else {
        resolve_sandbox_path(sandbox_path)
    };
    let mut binds = SESSION_BINDS.lock().map_err(|e| format!("lock error: {}", e))?;
    // Dedup by sandbox (in-sandbox) path.
    if let Some(pos) = binds.iter().position(|b| b.sandbox_path == sandbox) {
        binds[pos].mode = BindMode::Ro;
        binds[pos].host_path = host;
    } else {
        binds.push(SessionBind {
            host_path: host,
            sandbox_path: sandbox,
            mode: BindMode::Ro,
        });
    }
    Ok(())
}

/// Add a read-write path binding for the session.
///
/// `host_path` is resolved against the CWD (`~` expanded); it must exist.
/// `sandbox_path` is the in-sandbox destination (defaults to `host_path` when
/// empty).  Returns an error if the host path does not exist.
pub fn add_rw(host_path: &str, sandbox_path: &str) -> Result<(), String> {
    let host = path_util::resolve_canonical(host_path)?;
    let sandbox = if sandbox_path.is_empty() {
        host.clone()
    } else {
        resolve_sandbox_path(sandbox_path)
    };
    let mut binds = SESSION_BINDS.lock().map_err(|e| format!("lock error: {}", e))?;
    // Dedup by sandbox (in-sandbox) path.
    if let Some(pos) = binds.iter().position(|b| b.sandbox_path == sandbox) {
        binds[pos].mode = BindMode::Rw;
        binds[pos].host_path = host;
    } else {
        binds.push(SessionBind {
            host_path: host,
            sandbox_path: sandbox,
            mode: BindMode::Rw,
        });
    }
    Ok(())
}

/// Remove a path binding from the session.
///
/// `sandbox_path` is the in-sandbox path to remove (the "to" component).
/// Returns an error if no matching binding is found.
pub fn remove(sandbox_path: &str) -> Result<(), String> {
    let target = resolve_sandbox_path(sandbox_path);
    let mut binds = SESSION_BINDS.lock().map_err(|e| format!("lock error: {}", e))?;
    let pos = binds
        .iter()
        .position(|b| b.sandbox_path == target)
        .ok_or_else(|| format!("no session binding for sandbox path: {}", target.display()))?;
    binds.remove(pos);
    Ok(())
}

/// Return a copy of the current session bindings.
pub fn list() -> Vec<SessionBind> {
    SESSION_BINDS
        .lock()
        .map(|b| b.clone())
        .unwrap_or_default()
}

/// Merge session bindings into a `SandboxConfig` **clone**.
///
/// This appends the temporary bindings to the appropriate `ro`/`rw` lists.
/// The original config is not mutated; callers should clone their config
/// first and pass the clone here.
pub fn merge_into(cfg: &mut SandboxConfig) {
    let binds = list();

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

/// Clear all session bindings (used in tests).
#[cfg(test)]
pub fn clear() {
    if let Ok(mut b) = SESSION_BINDS.lock() {
        b.clear();
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
        assert!(binds[0].sandbox_path.to_string_lossy().ends_with("sandbox_ro"));
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
}
