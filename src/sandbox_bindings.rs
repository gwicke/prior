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

/// Resolve a host path (relative to CWD or absolute, `~` expanded) to an
/// absolute, canonical path.  Returns an error if the path does not exist.
fn resolve_host_path(path: &str) -> Result<PathBuf, String> {
    let cwd = std::env::current_dir().map_err(|e| format!("cannot get CWD: {}", e))?;
    let home = std::env::var_os("HOME").map(PathBuf::from);

    // Expand ~/ prefix
    let expanded = if let Some(rest) = path.strip_prefix("~/") {
        match &home {
            Some(h) => h.join(rest),
            None => return Err("cannot expand ~: HOME not set".into()),
        }
    } else if path == "~" {
        home.ok_or_else(|| String::from("cannot expand ~: HOME not set"))?
    } else {
        PathBuf::from(path)
    };

    let abs = if expanded.is_absolute() {
        expanded
    } else {
        cwd.join(&expanded)
    };

    // Normalize components (.., .)
    let normalized = normalize(&abs);

    // The path must exist so the binding is meaningful.
    let canonical = std::fs::canonicalize(&normalized)
        .map_err(|e| format!("path does not exist: {} ({})", normalized.display(), e))?;
    Ok(canonical)
}

/// Resolve a sandbox path (in-sandbox destination) — just normalizes, no
/// existence check required since it is created inside the namespace.
fn resolve_sandbox_path(path: &str) -> PathBuf {
    let expanded = if let Some(rest) = path.strip_prefix("~/") {
        let home = std::env::var_os("HOME").map(PathBuf::from);
        match &home {
            Some(h) => h.join(rest),
            None => PathBuf::from(path),
        }
    } else if path == "~" {
        std::env::var_os("HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from(path))
    } else {
        PathBuf::from(path)
    };
    normalize(&expanded)
}

fn normalize(p: &PathBuf) -> PathBuf {
    let mut result = PathBuf::new();
    for comp in p.components() {
        match comp {
            std::path::Component::ParentDir => { result.pop(); }
            std::path::Component::CurDir => {}
            c => result.push(c),
        }
    }
    result
}

/// Add a read-only path binding for the session.
///
/// `host_path` is resolved against the CWD (`~` expanded); it must exist.
/// `sandbox_path` is the in-sandbox destination (defaults to `host_path` when
/// empty).  Returns an error if the host path does not exist.
pub fn add_ro(host_path: &str, sandbox_path: &str) -> Result<(), String> {
    let host = resolve_host_path(host_path)?;
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
    let host = resolve_host_path(host_path)?;
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
    resolve_host_path(path).ok()
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
        let path = dir.path().join("never_added");
        fs::create_dir_all(&path).unwrap();
        let ps = path.to_string_lossy();

        let err = remove(&ps).unwrap_err();
        assert!(err.contains("no session binding"));
    }

    #[test]
    #[serial]
    fn test_add_nonexistent_path() {
        setup();
        let err = add_ro("/nonexistent_path_12345", "").unwrap_err();
        assert!(err.contains("does not exist"));
    }

    #[test]
    #[serial]
    fn test_merge_into_simple_and_remap() {
        setup();
        let dir = tempfile::tempdir().unwrap();
        let p1 = dir.path().join("merge_ro");
        let p2 = dir.path().join("merge_rw_host");
        let p2s = dir.path().join("merge_rw_sandbox");
        fs::create_dir_all(&p1).unwrap();
        fs::create_dir_all(&p2).unwrap();

        add_ro(&p1.to_string_lossy(), "").unwrap();
        add_rw(&p2.to_string_lossy(), &p2s.to_string_lossy()).unwrap();

        let mut cfg = SandboxConfig::default();
        assert!(cfg.ro.is_empty());
        assert!(cfg.rw.is_empty());

        merge_into(&mut cfg);

        assert_eq!(cfg.ro.len(), 1);
        assert_eq!(cfg.rw.len(), 1);

        // The simple bind uses BindPath::Simple
        match &cfg.ro[0] {
            crate::config::BindPath::Simple(s) => assert!(s.contains("merge_ro")),
            _ => panic!("expected Simple for ro bind"),
        }
        // The remap uses BindPath::Remap
        match &cfg.rw[0] {
            crate::config::BindPath::Remap { from, to } => {
                assert!(from.contains("merge_rw_host"));
                assert!(to.contains("merge_rw_sandbox"));
            }
            _ => panic!("expected Remap for rw bind"),
        }
    }
}