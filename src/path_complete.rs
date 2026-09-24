//! Filesystem completion entries for menu parameter leaves (and, later,
//! the in-prompt Tab complete — deferred).
//!
//! Host listing ([`list_host`]) and sandbox-aware listing
//! ([`list_sandbox`]: bwrap `ls -lap --time-style=+ISO` via
//! [`crate::tools::sandbox_list_lines`]) both yield structured
//! [`Entry`]s carrying mtime/size for the dialog's description column.
//! [`complete`] reproduces the v1 prefix split (ends-in-`/` or existing
//! dir ⇒ list its contents; else filter the parent by stem) with the
//! `~` round-trip — every consumer gets **full display paths** back
//! (that is what the row id needs).

use chrono::TimeZone;
use std::path::Path;
use std::time::{Duration, SystemTime};

/// One completable filesystem entry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    /// Full display path: dir-joined, `~/` preserved when the prefix
    /// used it, trailing `/` when `is_dir`. Used verbatim as the
    /// completion row id.
    pub path: String,
    /// True for directories (⇒ trailing `/` in `path`). Symlinks are
    /// never treated as dirs (matches `ls -p` / `file_type()`).
    pub is_dir: bool,
    /// Last modification time (second resolution).
    pub mtime: Option<SystemTime>,
    /// Size in bytes (raw, as `ls` reports).
    pub size: Option<u64>,
}

impl Entry {
    /// Description column: ISO-8601 local timestamp (seconds) + raw
    /// byte size. `-` for unavailable parts.
    pub fn description(&self) -> String {
        let ts = self
            .mtime
            .map(fmt_iso_secs)
            .unwrap_or_else(|| "-".to_string());
        let sz = self
            .size
            .map(|s| s.to_string())
            .unwrap_or_else(|| "-".to_string());
        format!("{}  {}", ts, sz)
    }
}

/// Complete `prefix` against the host FS (`sandbox = false`) or the
/// sandbox-visible FS with host fallback (`sandbox = true`, the v1
/// `complete_path_prefix_sandbox` semantics: sandbox listing wins;
/// while a sandbox is configured but the dir is invisible there,
/// return *nothing* rather than leaking host paths; fall back to the
/// host only when no sandbox is configured at all).
///
/// Empty prefix ⇒ empty (inert). Results are sorted by path.
pub fn complete(prefix: &str, sandbox: bool) -> Vec<Entry> {
    if prefix.is_empty() {
        return Vec::new();
    }
    let uses_tilde = prefix.starts_with('~');
    let expanded = crate::path_util::expand_tilde(prefix)
        .to_string_lossy()
        .into_owned();
    let path = Path::new(&expanded);
    let (dir, filter) = if expanded.ends_with('/') || path.is_dir() {
        let d = expanded.trim_end_matches('/');
        (
            if d.is_empty() { "/".to_string() } else { d.to_string() },
            String::new(),
        )
    } else {
        let parent = path
            .parent()
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or_default();
        let stem = path
            .file_name()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_default();
        (parent, stem)
    };

    let mut entries = if sandbox {
        match list_sandbox(&dir) {
            Some(entries) => entries,
            // Sandbox configured but the dir did not list inside it —
            // never suggest host paths the sandbox cannot see.
            None if crate::tools::sandbox_config().is_some() => return Vec::new(),
            // No sandbox at all → plain host listing.
            None => list_host(&dir),
        }
    } else {
        list_host(&dir)
    };

    if !filter.is_empty() {
        entries.retain(|e| basename_of(&e.path).starts_with(&filter));
    }
    if uses_tilde {
        let home = std::env::var("HOME").unwrap_or_default();
        for e in &mut entries {
            e.path = tilde_map(&e.path, &home);
        }
    }
    entries.sort_by(|a, b| a.path.cmp(&b.path));
    entries
}

/// List `dir` on the host filesystem. Metadata comes from the entry
/// itself (`file_type`/`metadata` do **not** follow symlinks — a
/// symlink-to-dir is not a dir here, matching `ls -p`).
pub fn list_host(dir: &str) -> Vec<Entry> {
    let mut out = Vec::new();
    let Ok(rd) = std::fs::read_dir(dir) else {
        return out;
    };
    for entry in rd.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        let is_dir = entry.file_type().map(|t| t.is_dir()).unwrap_or(false);
        let meta = entry.metadata().ok();
        out.push(Entry {
            path: join_path(dir, &name, is_dir),
            is_dir,
            mtime: meta.as_ref().and_then(|m| m.modified().ok()),
            size: meta.as_ref().map(|m| m.len()),
        });
    }
    out
}

/// List `dir` as the sandbox sees it. `None` when no sandbox is
/// configured, bwrap is unavailable, or `ls` fails inside (dir
/// invisible/nonexistent — the caller decides between "empty" and
/// "host fallback"; see [`complete`]).
pub fn list_sandbox(dir: &str) -> Option<Vec<Entry>> {
    let lines = crate::tools::sandbox_list_lines(dir)?;
    Some(parse_ls_long(&lines, dir))
}

/// Join `dir` + `name`, appending the trailing `/` for dirs.
fn join_path(dir: &str, name: &str, is_dir: bool) -> String {
    let base = dir.trim_end_matches('/');
    let mut p = if base.is_empty() {
        format!("/{}", name)
    } else {
        format!("{}/{}", base, name)
    };
    if is_dir && !p.ends_with('/') {
        p.push('/');
    }
    p
}

/// Basename of a display path (trailing `/` ignored).
fn basename_of(path: &str) -> &str {
    path.trim_end_matches('/').rsplit('/').next().unwrap_or(path)
}

/// Map a path under `home` to `~/…` (used when the typed prefix used
/// a tilde, so completion round-trips back into `~` form).
fn tilde_map(path: &str, home: &str) -> String {
    if home.is_empty() {
        return path.to_string();
    }
    if path == home {
        return "~".to_string();
    }
    let home_slash = if home.ends_with('/') {
        home.to_string()
    } else {
        format!("{}/", home)
    };
    match path.strip_prefix(&home_slash) {
        Some(rest) => format!("~/{}", rest),
        None => path.to_string(),
    }
}

/// Parse `ls -lap --time-style=+%Y-%m-%dT%H:%M:%S` output into entries.
/// Fields: perms links owner group size datetime name… (name may
/// contain spaces; `-p` may append `/` to dirs; `ls -l` renders
/// symlinks as `name -> target`). Skips `total …`, `.`, and `..`.
fn parse_ls_long(lines: &[String], dir: &str) -> Vec<Entry> {
    let mut out = Vec::new();
    for line in lines {
        let line = line.trim_end();
        if line.is_empty() || line.starts_with("total ") {
            continue;
        }
        let mut it = line.split_whitespace();
        let (Some(perms), Some(_links), Some(_owner), Some(_group), Some(size), Some(dt)) =
            (it.next(), it.next(), it.next(), it.next(), it.next(), it.next())
        else {
            continue;
        };
        let rest: Vec<&str> = it.collect();
        if rest.is_empty() {
            continue;
        }
        let mut name = rest.join(" ");
        if perms.starts_with('l') {
            if let Some((n, _)) = name.split_once(" -> ") {
                name = n.to_string();
            }
        }
        let name = name.trim_end_matches('/');
        if name.is_empty() || name == "." || name == ".." {
            continue;
        }
        let is_dir = perms.starts_with('d');
        out.push(Entry {
            path: join_path(dir, name, is_dir),
            is_dir,
            mtime: parse_iso_local(dt),
            size: size.parse::<u64>().ok(),
        });
    }
    out
}

/// Parse the `--time-style=+%Y-%m-%dT%H:%M:%S` token (ls prints local
/// time) back into a `SystemTime`.
fn parse_iso_local(s: &str) -> Option<SystemTime> {
    let naive = chrono::NaiveDateTime::parse_from_str(s, "%Y-%m-%dT%H:%M:%S").ok()?;
    let dt = chrono::Local
        .from_local_datetime(&naive)
        .single()?;
    system_time_from_timestamp(dt.timestamp())
}

/// `SystemTime` ⇄ unix seconds.
fn system_time_from_timestamp(ts: i64) -> Option<SystemTime> {
    if ts >= 0 {
        SystemTime::UNIX_EPOCH.checked_add(Duration::from_secs(ts as u64))
    } else {
        SystemTime::UNIX_EPOCH.checked_sub(Duration::from_secs((-ts) as u64))
    }
}

/// ISO-8601 local seconds — same format `ls --time-style=+…` prints.
fn fmt_iso_secs(t: SystemTime) -> String {
    chrono::DateTime::<chrono::Local>::from(t)
        .format("%Y-%m-%dT%H:%M:%S")
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::path::PathBuf;

    fn write(path: &PathBuf, content: &str) {
        fs::write(path, content).unwrap();
    }

    #[test]
    fn empty_prefix_is_inert() {
        assert!(complete("", false).is_empty());
    }

    #[test]
    fn host_dir_listing_full_paths_sorted_with_metadata() {
        let dir = tempfile::tempdir().unwrap();
        write(&dir.path().join("a.txt"), "hello"); // 5 bytes
        fs::create_dir(dir.path().join("sub")).unwrap();
        let prefix = format!("{}/", dir.path().display());

        let entries = complete(&prefix, false);
        assert_eq!(entries.len(), 2, "{entries:?}");

        let a = entries.iter().find(|e| e.path.ends_with("a.txt")).unwrap();
        let s = entries.iter().find(|e| e.path.ends_with("sub/")).unwrap();
        assert!(!a.is_dir, "file must not carry a slash: {a:?}");
        assert!(s.is_dir, "dir must carry a slash: {s:?}");
        // Full display path, starts with the typed (expanded) prefix.
        assert!(
            a.path.starts_with(dir.path().to_str().unwrap()),
            "{a:?}"
        );
        // Sorted by path.
        let paths: Vec<&str> = entries.iter().map(|e| e.path.as_str()).collect();
        let mut sorted = paths.clone();
        sorted.sort();
        assert_eq!(paths, sorted);

        // Metadata: 5-byte file, real mtime, description = ISO + size.
        assert_eq!(a.size, Some(5));
        assert!(a.mtime.is_some());
        let d = a.description();
        // "YYYY-MM-DDTHH:MM:SS  5"
        assert_eq!(d.len(), 19 + 2 + 1, "{d}");
        assert!(d.contains("T"), "{d}");
        assert!(d.ends_with("  5"), "{d}");

        // Stem filter: parent + prefix-of-basename.
        let stem = complete(&format!("{}/a", dir.path().display()), false);
        assert_eq!(stem.len(), 1, "{stem:?}");
        assert!(stem[0].path.ends_with("a.txt"), "{stem:?}");
    }

    #[test]
    fn host_existing_dir_without_slash_lists_children() {
        // v1 quirk preserved: an existing dir prefix lists its contents
        // even without a trailing slash.
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir(dir.path().join("sub")).unwrap();
        write(&dir.path().join("sub/x"), "y");
        let entries = complete(&format!("{}/sub", dir.path().display()), false);
        assert_eq!(entries.len(), 1, "{entries:?}");
        assert!(entries[0].path.ends_with("sub/x"));
    }

    #[test]
    fn tilde_round_trip_mapping() {
        assert_eq!(tilde_map("/home/u/proj/", "/home/u"), "~/proj/");
        assert_eq!(tilde_map("/home/u", "/home/u"), "~");
        assert_eq!(tilde_map("/etc/x", "/home/u"), "/etc/x");
        // Path-aligned prefix only — /home/u2 is not under /home/u.
        assert_eq!(tilde_map("/home/u2/x", "/home/u"), "/home/u2/x");
        // No HOME → no mapping.
        assert_eq!(tilde_map("/home/u/x", ""), "/home/u/x");
    }

    #[test]
    fn parse_ls_long_shapes() {
        let lines: Vec<String> = [
            "total 42",
            // "." / ".." (with -p slash) must be skipped.
            "drwxr-xr-x  1 1000 1000     1187 2025-01-15T09:30:00 ./",
            "drwxr-xr-x 11 1000 1000      220 2025-01-14T22:11:05 ../",
            "drwxr-xr-x  2 1000 1000     4096 2025-01-14T22:11:05 sub",
            "-rw-r--r--  1 1000 1000        5 2025-01-15T10:00:00 file with spaces.txt",
            "lrwxrwxrwx  1 1000 1000        7 2024-12-01T10:00:00 link -> target",
            "lrwxrwxrwx  1 1000 1000       11 2024-12-01T10:00:00 dirlink -> sub",
        ]
        .iter()
        .map(|s| s.to_string())
        .collect();

        let entries = parse_ls_long(&lines, "/base");
        assert_eq!(entries.len(), 4, "{entries:?}");

        let sub = entries.iter().find(|e| e.path.ends_with("sub/")).unwrap();
        assert!(sub.is_dir);
        assert_eq!(sub.size, Some(4096));
        assert_eq!(
            sub.mtime,
            parse_iso_local("2025-01-14T22:11:05"),
            "mtime must round-trip through the ls token"
        );

        let f = entries
            .iter()
            .find(|e| e.path.ends_with("file with spaces.txt"))
            .unwrap();
        assert!(!f.is_dir);
        assert_eq!(f.size, Some(5));

        // Symlink: target stripped, never a dir (even `dirlink`).
        let link = entries.iter().find(|e| e.path.ends_with("/link")).unwrap();
        assert!(!link.is_dir);
        assert!(!link.path.contains("->"), "{link:?}");
        let dirlink = entries
            .iter()
            .find(|e| e.path.ends_with("/dirlink"))
            .unwrap();
        assert!(!dirlink.is_dir, "symlink-to-dir must not be a dir: {dirlink:?}");

        // Description of a parsed entry: ISO + raw size.
        let d = sub.description();
        assert!(d.starts_with("2025-01-14T22:11:05"), "{d}");
        assert!(d.ends_with("  4096"), "{d}");
    }

    #[test]
    #[serial_test::serial]
    fn sandbox_flag_falls_back_to_host_when_no_sandbox() {
        crate::tools::set_sandbox(None);
        let dir = tempfile::tempdir().unwrap();
        write(&dir.path().join("f"), "x");
        let entries = complete(&format!("{}/", dir.path().display()), true);
        assert!(!entries.is_empty(), "host fallback expected: {entries:?}");
        assert!(entries[0].path.ends_with("/f"), "{entries:?}");
    }

    #[test]
    #[serial_test::serial]
    fn sandbox_active_but_dir_invisible_yields_empty() {
        crate::tools::set_sandbox(Some(crate::config::SandboxConfig::default()));
        let dir = tempfile::tempdir().unwrap();
        write(&dir.path().join("f"), "x");
        // Default sandbox: deny-by-default — the tempdir is not visible
        // (bwrap absent ⇒ ls fails; present ⇒ tmpfs /tmp hides it).
        // Either way: empty, never host entries.
        let entries = complete(&format!("{}/", dir.path().display()), true);
        assert!(entries.is_empty(), "must not leak host paths: {entries:?}");
        crate::tools::set_sandbox(None);
    }
}
