//! Unified path resolution utilities.
//!
//! Consolidates `~` expansion, relative-path resolution, and path normalization
//! that used to be duplicated across [`config`](crate::config),
//! [`sandbox_bindings`](crate::sandbox_bindings), and [`repl`](crate::repl).

use std::path::{Path, PathBuf};

/// Expand `~` or `~/` prefix to the home directory, returning a `PathBuf`.
///
/// This is a thin wrapper around [`shellexpand::tilde`] that converts the
/// resulting `Cow<str>` into a `PathBuf`.  If `HOME` is not set the path is
/// returned unchanged.
pub fn expand_tilde(path: &str) -> PathBuf {
    PathBuf::from(shellexpand::tilde(path).as_ref())
}

/// Resolve a (possibly relative or `~`-prefixed) path against `cwd` / home.
///
/// 1. `~`/`~/` is expanded to the home directory (via [`expand_tilde`]).
/// 2. If the result is relative, it is joined with `cwd`.
///
/// Unlike [`resolve_canonical`] this does **not** touch the filesystem.
pub fn resolve(path: &str, cwd: &Path) -> PathBuf {
    let expanded = expand_tilde(path);
    if expanded.is_absolute() {
        expanded
    } else {
        cwd.join(expanded)
    }
}

/// Like [`resolve`] but accepts an explicit `home` override.
///
/// When `home` is `Some` the passed value is used for `~` expansion instead of
/// reading `HOME` from the environment.  This is useful in tests.
pub fn resolve_with_home(path: &str, cwd: &Path, home: Option<&Path>) -> PathBuf {
    let expanded = match home {
        Some(h) => {
            if path == "~" {
                h.to_path_buf()
            } else if let Some(rest) = path.strip_prefix("~/") {
                h.join(rest)
            } else {
                PathBuf::from(path)
            }
        }
        None => expand_tilde(path),
    };
    if expanded.is_absolute() {
        expanded
    } else {
        cwd.join(expanded)
    }
}

/// Resolve a path and normalize `..` / `.` components.
///
/// Convenience shortcut for `normalize(&resolve(path, cwd))`.
pub fn resolve_normalized(path: &str, cwd: &Path) -> PathBuf {
    normalize(&resolve(path, cwd))
}

/// Fully resolve a host path:
/// tilde expansion → relative resolution → normalize → canonicalize (fs).
///
/// Returns an error if the path does not exist (canonicalize fails).
pub fn resolve_canonical(path: &str) -> Result<PathBuf, String> {
    let cwd = std::env::current_dir().map_err(|e| format!("cannot get CWD: {e}"))?;
    let resolved = resolve_normalized(path, &cwd);
    std::fs::canonicalize(&resolved)
        .map_err(|e| format!("path does not exist: {} ({e})", resolved.display()))
}

/// Normalize `..` / `.` components without touching the filesystem.
///
/// Delegates to [`path_clean::clean`] under the hood.
pub fn normalize(p: &Path) -> PathBuf {
    path_clean::clean(p)
}
