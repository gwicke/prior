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
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionBind {
    pub host_path: PathBuf,
    pub mode: BindMode,
}

/// Global session-scoped additional bindings.
static SESSION_BINDS: LazyLock<Mutex<Vec<SessionBind>>> =
    LazyLock::new(|| Mutex::new(Vec::new()));

/// Resolve `path` (relative to CWD or absolute, `~` expanded) to an absolute
/// host path, then canonicalize if it exists. Returns the normalized path on
/// success, or an error string if the path does not exist.
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

    // The path must exist (or be a valid prefix of an existing path) so the
    // binding is meaningful. We canonicalize for the existence check but
    // store the normalized (non-canonical) form so the user sees their own
    // path string in the list.
    let canonical = std::fs::canonicalize(&normalized)
        .map_err(|e| format!("path does not exist: {} ({})", normalized.display(), e))?;
    // If canonicalization resolved symlinks to a different path, use the
    // canonical form so bwrap gets the real path.
    Ok(canonical)
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
/// `path` is resolved against the CWD (`~` expanded). Returns an error if the
/// path does not exist or cannot be resolved.
pub fn add_ro(path: &str) -> Result<(), String> {
    let host_path = resolve_host_path(path)?;
    let mut binds = SESSION_BINDS.lock().map_err(|e| format!("lock error: {}", e))?;
    // Dedup: replace an existing binding for the same path (regardless of mode).
    if let Some(pos) = binds.iter().position(|b| b.host_path == host_path) {
        binds[pos].mode = BindMode::Ro;
    } else {
        binds.push(SessionBind {
            host_path,
            mode: BindMode::Ro,
        });
    }
    Ok(())
}

/// Add a read-write path binding for the session.
///
/// `path` is resolved against the CWD (`~` expanded). Returns an error if the
/// path does not exist or cannot be resolved.
pub fn add_rw(path: &str) -> Result<(), String> {
    let host_path = resolve_host_path(path)?;
    let mut binds = SESSION_BINDS.lock().map_err(|e| format!("lock error: {}", e))?;
    // Dedup: replace an existing binding for the same path (regardless of mode).
    if let Some(pos) = binds.iter().position(|b| b.host_path == host_path) {
        binds[pos].mode = BindMode::Rw;
    } else {
        binds.push(SessionBind {
            host_path,
            mode: BindMode::Rw,
        });
    }
    Ok(())
}

/// Remove a path binding from the session.
///
/// `path` is resolved the same way as `add_ro`/`add_rw`. Returns an error if
/// the resolved path does not match any existing binding.
pub fn remove(path: &str) -> Result<(), String> {
    let host_path = resolve_host_path(path)?;
    let mut binds = SESSION_BINDS.lock().map_err(|e| format!("lock error: {}", e))?;
    let pos = binds
        .iter()
        .position(|b| b.host_path == host_path)
        .ok_or_else(|| format!("no session binding for path: {}", host_path.display()))?;
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
        // Convert to a BindPath::Simple using the host path. Since the path
        // is already absolute and resolved, we use the string form.
        let from = b.host_path.to_string_lossy().to_string();
        // Map to in-sandbox path: same as host (simple bind).
        match b.mode {
            BindMode::Ro => cfg.ro.push(crate::config::BindPath::Simple(from)),
            BindMode::Rw => cfg.rw.push(crate::config::BindPath::Simple(from)),
        }
    }
}

/// Helper: resolve a path for display/removal, returning the normalized form
/// (the same key used in the list). Returns `None` if the path does not exist.
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
    use std::fs;

    fn setup() {
        clear();
    }

    #[test]
    fn test_add_ro_and_list() {
        setup();
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test_ro");
        fs::create_dir_all(&path).unwrap();

        let ps = path.to_string_lossy();
        add_ro(&ps).unwrap();
        let binds = list();
        assert_eq!(binds.len(), 1);
        assert_eq!(binds[0].mode, BindMode::Ro);
        assert!(binds[0].host_path.exists());

        // Add same path again (dedup, still ro)
        add_ro(&ps).unwrap();
        assert_eq!(list().len(), 1);
    }

    #[test]
    fn test_add_rw_and_list() {
        setup();
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test_rw");
        fs::create_dir_all(&path).unwrap();

        let ps = path.to_string_lossy();
        add_rw(&ps).unwrap();
        let binds = list();
        assert_eq!(binds.len(), 1);
        assert_eq!(binds[0].mode, BindMode::Rw);
    }

    #[test]
    fn test_dedup_upgrades_mode() {
        setup();
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("dedup");
        fs::create_dir_all(&path).unwrap();

        let ps = path.to_string_lossy();
        add_ro(&ps).unwrap();
        // Upgrade to rw
        add_rw(&ps).unwrap();
        let binds = list();
        assert_eq!(binds.len(), 1);
        assert_eq!(binds[0].mode, BindMode::Rw);
    }

    #[test]
    fn test_remove() {
        setup();
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("remove_me");
        fs::create_dir_all(&path).unwrap();

        let ps = path.to_string_lossy();
        add_ro(&ps).unwrap();
        assert_eq!(list().len(), 1);
        remove(&ps).unwrap();
        assert_eq!(list().len(), 0);
    }

    #[test]
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
    fn test_add_nonexistent_path() {
        setup();
        let err = add_ro("/nonexistent_path_12345").unwrap_err();
        assert!(err.contains("does not exist"));
    }

    #[test]
    fn test_merge_into() {
        setup();
        let dir = tempfile::tempdir().unwrap();
        let p1 = dir.path().join("merge_ro");
        let p2 = dir.path().join("merge_rw");
        fs::create_dir_all(&p1).unwrap();
        fs::create_dir_all(&p2).unwrap();

        add_ro(&p1.to_string_lossy()).unwrap();
        add_rw(&p2.to_string_lossy()).unwrap();

        let mut cfg = SandboxConfig::default();
        assert!(cfg.ro.is_empty());
        assert!(cfg.rw.is_empty());

        merge_into(&mut cfg);

        assert_eq!(cfg.ro.len(), 1);
        assert_eq!(cfg.rw.len(), 1);
    }
}