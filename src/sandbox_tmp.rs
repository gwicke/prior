//! The per-session `/tmp` directory.
//!
//! `/tmp` inside the sandbox is a host directory named for the session, bound
//! over `/tmp` (see `doc/sandbox.md` → "`/tmp` is a per-session directory").
//! Two properties matter and both are decided by *one* resolver:
//!
//! - **Persistence.** The directory outlives a single command, so two `bash`
//!   calls in one session see each other's `/tmp` files.
//! - **Isolation.** It is named for the session id, so nothing else — not
//!   another session, not another cwd, not the host `/tmp` — is visible.
//!
//! The shell (bwrap argv) and the non-shell file tools ([`crate::config`]'s
//! `PathPolicy`) must resolve `/tmp` to the *same* host path, or `bash` would
//! create a file that `write`/`read` cannot see. [`resolve`] is that single
//! resolution, and both call it.

use std::path::{Path, PathBuf};
use std::sync::{LazyLock, Mutex};

use crate::config::{resolve_paths, BindMode, ResolvedBind, SandboxConfig};

/// Prefix on every session directory, so a stale one is identifiable (and
/// prunable) without knowing prior's internals, and so the sweeper can list
/// exactly the entries that belong to prior inside a possibly shared parent.
const PREFIX: &str = "prior-";

/// How long an untouched session directory survives the sweep. A week is the
/// right order of magnitude: a session left open overnight is safe, and the
/// directory does not accumulate.
const MAX_AGE: std::time::Duration = std::time::Duration::from_secs(7 * 24 * 60 * 60);

/// The live session id, mirrored here so the sandbox layer (which has no
/// `AppState` handle) can name the directory. Kept current by
/// [`crate::app_state::AppState`] on every session switch.
static SESSION_ID: LazyLock<Mutex<String>> = LazyLock::new(|| Mutex::new(String::new()));

/// Publish the current session id. Called by `AppState` whenever the current
/// session changes (`/clear`, `/session … resume`, `/new`, startup).
pub fn set_session_id(id: impl Into<String>) {
    if let Ok(mut slot) = SESSION_ID.lock() {
        *slot = id.into();
    }
}

/// The current session id, or the empty string when there is none (one-shot
/// runs, tests).
pub fn session_id() -> String {
    SESSION_ID.lock().map(|s| s.clone()).unwrap_or_default()
}

/// Directory name for a session: `prior-<session-uuid>`.
pub fn dir_name(session_id: &str) -> String {
    format!("{PREFIX}{session_id}")
}

/// Whether `name` is one of prior's session tmp directories, and if so the
/// session id it names.
fn session_id_of(name: &str) -> Option<&str> {
    name.strip_prefix(PREFIX).filter(|id| !id.is_empty())
}

/// The directory a session's `/tmp` lives under, before the session name is
/// appended.
///
/// A binding that remaps `/tmp` (`to == "/tmp"`) wins: its host side is the
/// base, so the operator's choice of filesystem is respected while the session
/// subdirectory keeps sessions apart. With no such binding the base is
/// `<app_config_dir>/sandbox-tmp`, which survives a reboot and `/tmp` cleaner —
/// unlike a directory under the host `/tmp`, which is exactly what we are
/// isolating from.
///
/// A binding whose `to` is `/tmp` but whose `from` is a *file* leaves no
/// writable `/tmp` at all; that is rejected rather than silently producing a
/// namespace with no usable scratch space.
fn base_from_binds(binds: &[ResolvedBind]) -> Result<Option<PathBuf>, String> {
    let tmp = Path::new("/tmp");
    if let Some(b) = binds.iter().find(|b| b.to == tmp) {
        // Reject only an *existing file* — a directory, or a path that does not
        // exist yet (which `ensure` will create as the session directory's
        // parent), is fine. Rejecting at the first spawn rather than the first
        // write gives a clear message instead of a bwrap "can't find source".
        if b.from.is_file() {
            return Err(format!(
                "sandbox /tmp binding '{}' is a file: /tmp inside the sandbox is a \
                 per-session directory created under a directory there",
                b.from.display()
            ));
        }
        return Ok(Some(b.from.clone()));
    }
    // No config directory (no `HOME`/`XDG_CONFIG_HOME`) is not an error: there
    // is simply nowhere durable to keep the session directory, so the caller
    // falls back to the throwaway tmpfs. Returning `None` says exactly that; a
    // sandbox must still run.
    Ok(crate::platform::app_config_dir().map(|d| d.join("sandbox-tmp")))
}

/// The base for a config alone — used by cleanup, which runs outside a spawn
/// and so has no already-resolved bindings to hand.
fn base_for_config(cfg: &SandboxConfig) -> Result<Option<PathBuf>, String> {
    let cwd = std::env::current_dir().map_err(|e| e.to_string())?;
    let home = std::env::var_os("HOME").map(PathBuf::from);
    let binds = resolve_paths(&cwd, home.as_deref(), &cfg.ro, &cfg.rw)?;
    base_from_binds(&binds)
}

/// Create the session directory (mode `0700`) under `base` and return its path.
///
/// Creation has to happen in the **parent**: bwrap hard-errors on a missing
/// bind source (`Can't find source path`), and the sandbox cannot create its
/// own mount source. Idempotent, so both the argv builder and the policy
/// builder may call it.
fn ensure(base: &Path, session_id: &str) -> std::io::Result<PathBuf> {
    let dir = base.join(dir_name(session_id));
    std::fs::create_dir_all(&dir)?;
    #[cfg(unix)]
    std::fs::set_permissions(&dir, std::os::unix::fs::PermissionsExt::from_mode(0o700))?;
    Ok(dir)
}

/// The binding that makes the session directory the sandbox's `/tmp`.
pub fn bind(dir: &Path) -> ResolvedBind {
    ResolvedBind {
        mode: BindMode::Rw,
        from: dir.to_path_buf(),
        to: PathBuf::from("/tmp"),
    }
}

/// Resolve the host directory that should be bound at `/tmp` for the current
/// session, given the resolved bindings, creating it if needed.
///
/// Returns `Ok(None)` when there is no session to name it after: `--input` /
/// recipe one-shot runs have none, and persist nothing between two commands
/// anyway, so they keep the plain tmpfs.
///
/// The `to = "/tmp"` binding, if any, is read as the **base** — the session
/// directory is created under it and the caller must replace that binding with
/// [`bind`] of the returned path.
pub fn resolve(binds: &[ResolvedBind]) -> Result<Option<PathBuf>, String> {
    resolve_id(binds, &session_id())
}

/// [`resolve`] with an explicit session id, so the id source is a parameter and
/// tests need not mutate the process-global.
fn resolve_id(binds: &[ResolvedBind], id: &str) -> Result<Option<PathBuf>, String> {
    if id.is_empty() {
        return Ok(None);
    }
    match base_from_binds(binds)? {
        Some(base) => ensure(&base, id).map(Some).map_err(|e| e.to_string()),
        None => Ok(None),
    }
}

/// Map an agent-visible `/tmp[/…]` path to the host file under the session
/// directory, for the file tools when there are no explicit bindings (the
/// permissive default, where `PathPolicy` is absent and paths are otherwise
/// cwd-scoped).
///
/// Without this the two worlds disagree in the worst way: the shell writes
/// `/tmp/f` into the session directory while the `write` tool either rejects
/// the path (outside the cwd) or writes somewhere else. Returns `None` for any
/// path that is not under `/tmp`, so the caller falls through to its normal
/// cwd check.
pub fn tool_path(agent_path: &str) -> Option<PathBuf> {
    let id = session_id();
    if id.is_empty() {
        return None;
    }
    let rel = Path::new(agent_path).strip_prefix("/tmp").ok()?;
    let base = base_from_binds(&[]).ok()??;
    let dir = ensure(&base, &id).ok()?;
    // Normalize before joining so `..` cannot climb out of the session
    // directory and into a sibling or the host filesystem.
    let host = crate::path_util::normalize(&dir.join(rel));
    host.starts_with(&dir).then_some(host)
}

/// Remove a session's `/tmp` directory. Best-effort, always: a leaked
/// directory is caught by [`sweep`], whereas an error that aborted `/clear`
/// would cost the user their session — a far worse trade.
pub fn remove(cfg: &SandboxConfig, session_id: &str) {
    if session_id.is_empty() {
        return;
    }
    let Ok(Some(base)) = base_for_config(cfg) else {
        return;
    };
    let _ = std::fs::remove_dir_all(base.join(dir_name(session_id)));
}

/// Delete session directories not touched for [`MAX_AGE`], for the cases the
/// two explicit removal points cannot reach: a session deleted while prior was
/// not running, a crash between the file write and the directory removal, a
/// directory whose session file was removed by hand.
///
/// Only `prior-<uuid>` entries are touched, whatever else shares the parent —
/// which is the point of the prefix. Best-effort: every error is ignored.
pub fn sweep(cfg: &SandboxConfig) {
    let Ok(Some(base)) = base_for_config(cfg) else {
        return;
    };
    let Ok(entries) = std::fs::read_dir(&base) else {
        return;
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(name) = name.to_str() else { continue };
        if session_id_of(name).is_none() {
            continue;
        }
        let Ok(meta) = entry.metadata() else { continue };
        if !meta.is_dir() {
            continue;
        }
        let fresh = meta
            .modified()
            .ok()
            .and_then(|m| m.elapsed().ok())
            .is_some_and(|age| age < MAX_AGE);
        if !fresh {
            let _ = std::fs::remove_dir_all(entry.path());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dir_name_is_prefixed() {
        assert_eq!(dir_name("abc-123"), "prior-abc-123");
        assert_eq!(session_id_of("prior-abc-123"), Some("abc-123"));
        assert_eq!(session_id_of("unrelated"), None);
        assert_eq!(session_id_of("prior-"), None);
    }

    #[test]
    fn base_prefers_a_tmp_remap() {
        let t = tempfile::tempdir().unwrap();
        let binds = vec![ResolvedBind {
            mode: BindMode::Rw,
            from: t.path().to_path_buf(),
            to: PathBuf::from("/tmp"),
        }];
        assert_eq!(
            base_from_binds(&binds).unwrap(),
            Some(t.path().to_path_buf())
        );
    }

    #[test]
    fn base_rejects_a_tmp_file_binding() {
        // A file remap leaves no writable /tmp at all; say so instead of
        // emitting a namespace with nowhere to write.
        let t = tempfile::tempdir().unwrap();
        let file = t.path().join("not-a-dir");
        std::fs::write(&file, b"x").unwrap();
        let binds = vec![ResolvedBind {
            mode: BindMode::Rw,
            from: file,
            to: PathBuf::from("/tmp"),
        }];
        let err = base_from_binds(&binds).unwrap_err();
        assert!(err.contains("is a file"), "{err}");
    }

    #[test]
    fn base_accepts_a_not_yet_existing_remap_dir() {
        // A remap to a directory prior will create is fine; only an existing
        // file is a misconfiguration.
        let t = tempfile::tempdir().unwrap();
        let dir = t.path().join("will-be-created");
        let binds = vec![ResolvedBind {
            mode: BindMode::Rw,
            from: dir.clone(),
            to: PathBuf::from("/tmp"),
        }];
        assert_eq!(base_from_binds(&binds).unwrap(), Some(dir));
    }

    #[test]
    fn no_session_means_no_directory() {
        // One-shot runs (no session id) keep the plain tmpfs.
        assert!(resolve_id(&[], "").unwrap().is_none());
    }

    #[test]
    fn a_session_makes_a_directory_under_the_base() {
        let t = tempfile::tempdir().unwrap();
        let binds = vec![ResolvedBind {
            mode: BindMode::Rw,
            from: t.path().to_path_buf(),
            to: PathBuf::from("/tmp"),
        }];
        let dir = resolve_id(&binds, "sess-1").unwrap().unwrap();
        assert_eq!(dir, t.path().join("prior-sess-1"));
        // Under the operator's remap base, not beside it.
        assert!(dir.starts_with(t.path()));
    }

    #[test]
    fn ensure_creates_a_0700_directory_idempotently() {
        use std::os::unix::fs::PermissionsExt;
        let t = tempfile::tempdir().unwrap();
        let dir = ensure(t.path(), "sess").unwrap();
        assert_eq!(dir, t.path().join("prior-sess"));
        let mode = std::fs::metadata(&dir).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o700);
        // Second call is a no-op, not an error.
        assert_eq!(ensure(t.path(), "sess").unwrap(), dir);
    }
}
