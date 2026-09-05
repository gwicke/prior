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

/// Render `path` relative to `cwd` for display in tool output. When
/// `path` is under `cwd` (lexically — we use string-prefix matching to stay
/// consistent with the cwd reported in the system prompt and with the path
/// bounds already enforced by `validate_path`), the absolute prefix is
/// dropped: paths equal to `cwd` become `.`; children become `./child`.
///
/// Paths that are NOT under `cwd` are returned as-is. This is deliberate:
/// the policy is "sub-paths of cwd become relative", not "all absolute
/// paths become relative". External paths (e.g. `/usr/bin/foo`, `/tmp/x`)
/// stay absolute so the model (and the human reading the scrollback) sees
/// them for what they are. Going further and rewriting them to `../...`
/// would obscure where they actually live.
///
/// Examples (cwd = `/home/u/proj`):
///
/// * `/home/u/proj`             -> `.`
/// * `/home/u/proj/src/main.rs` -> `./src/main.rs`
/// * `/home/u/proj2/x`          -> `/home/u/proj2/x`  (sibling, common prefix — NOT under cwd)
/// * `/etc/passwd`              -> `/etc/passwd`
///
/// Lexical prefixing has one edge case the user should know about: when
/// `cwd` itself is a symlink (e.g. `/home/u/proj` -> `/var/data/proj`),
/// paths resolved through `get_path` may carry the canonical form
/// `/var/data/proj/...` and therefore won't be considered "under cwd" by
/// this helper. That's the same boundary `validate_path` uses, so the two
/// stay consistent; the only practical consequence is that such paths come
/// back absolute instead of relative.
pub fn relativize_path(path: &Path, cwd: &Path) -> String {
    let path_str = path.as_os_str().to_string_lossy();
    let cwd_str = cwd.as_os_str().to_string_lossy();
    if cwd_str.is_empty() {
        return path_str.into_owned();
    }
    let p = path_str.as_ref();
    if p == cwd_str {
        return ".".to_string();
    }
    // Must be `cwd + "/..."` — a bare lexical prefix match without the
    // separator would let `/home/u/proj2` claim a relationship to
    // `/home/u/proj`. `strip_prefix` on the cwd string leaves either
    // `/...` (a child path) or "" (exact cwd, caught above); anything
    // else (e.g. `"2/x"` from `/home/u/proj2/x`) means the bytes shared
    // a prefix but the path is actually a sibling — fall through and
    // return the original absolute form.
    if let Some(rest) = p.strip_prefix(&*cwd_str) {
        if let Some(child) = rest.strip_prefix('/') {
            return format!("./{child}");
        }
        if rest.is_empty() {
            // Defensive: should have been caught by the `p == cwd_str`
            // branch above, but keep the behaviour explicit if some
            // platform's `to_string_lossy` ever produces a trailing
            // separator on cwd.
            return ".".to_string();
        }
    }
    path_str.into_owned()
}

#[cfg(test)]
mod relativize_tests {
    use super::*;

    fn cwd(s: &str) -> PathBuf {
        PathBuf::from(s)
    }

    #[test]
    fn exact_cwd_becomes_dot() {
        let cwd = cwd("/home/u/proj");
        assert_eq!(relativize_path(Path::new("/home/u/proj"), &cwd), ".");
    }

    #[test]
    fn child_path_becomes_dot_slash() {
        let cwd = cwd("/home/u/proj");
        assert_eq!(
            relativize_path(Path::new("/home/u/proj/src/main.rs"), &cwd),
            "./src/main.rs",
        );
        assert_eq!(
            relativize_path(Path::new("/home/u/proj/a"), &cwd),
            "./a",
        );
    }

    #[test]
    fn deeply_nested_child_keeps_full_tail() {
        let cwd = cwd("/home/u/proj");
        assert_eq!(
            relativize_path(
                Path::new("/home/u/proj/src/sub/mod.rs"),
                &cwd
            ),
            "./src/sub/mod.rs",
        );
    }

    #[test]
    fn sibling_with_common_prefix_left_alone() {
        // `/home/u/proj2/x` shares the cwd's prefix lexically but is NOT
        // under cwd. Must round-trip unchanged — otherwise we'd silently
        // misattribute sibling files to cwd.
        let cwd = cwd("/home/u/proj");
        assert_eq!(
            relativize_path(Path::new("/home/u/proj2/x"), &cwd),
            "/home/u/proj2/x",
        );
        assert_eq!(
            relativize_path(Path::new("/home/u/proj-other/x"), &cwd),
            "/home/u/proj-other/x",
        );
    }

    #[test]
    fn paths_above_or_beside_cwd_left_alone() {
        let cwd = cwd("/home/u/proj");
        assert_eq!(
            relativize_path(Path::new("/etc/passwd"), &cwd),
            "/etc/passwd",
        );
        assert_eq!(
            relativize_path(Path::new("/home/u/other/x"), &cwd),
            "/home/u/other/x",
        );
        assert_eq!(
            relativize_path(Path::new("/tmp/foo"), &cwd),
            "/tmp/foo",
        );
    }

    #[test]
    fn empty_cwd_is_passthrough() {
        // Defensive: an empty cwd would otherwise match every string and
        // break the prefix-strip below.
        let cwd = cwd("");
        assert_eq!(
            relativize_path(Path::new("/home/u/proj/foo"), &cwd),
            "/home/u/proj/foo",
        );
    }
}
