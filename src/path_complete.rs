//! Filesystem completion entries for menu parameter leaves and the
//! in-prompt (prose) completion — see
//! `doc/slashdot/arg_and_modifier_autocomplete.md` §Prompt-line
//! completion.
//!
//! Host listing ([`list_host`]) and sandbox-aware listing
//! ([`list_sandbox`]: bwrap `ls -lap --time-style=+ISO` via
//! [`crate::tools::sandbox_list_lines`]) both yield structured
//! [`Entry`]s carrying mtime/size for the dialog's description column.
//! [`complete`] reproduces the v1 prefix split (ends-in-`/` or existing
//! dir ⇒ list its contents; empty parent ⇒ `.`; else the parent with
//! the basename as stem) with the `~` round-trip, then ranks the
//! path-sorted children with [`crate::fuzzy::rank_candidates`]
//! (query = stem: dir level ⇒ alphabetical, typed stem ⇒ prefix tier
//! then fuzzy) — every consumer gets **full display paths** back (that
//! is what the row id needs).
//!
//! Listings are cached in a shared single slot (L1): the host side
//! revalidates with one dir-`mtime` stat per call, the sandbox side
//! with a TTL (a probe would be a bwrap spawn). See [`listing_stale`].

use crate::fuzzy::rank_candidates;
use crate::slashdot_menu::node_impl::CompletionRows;
use crate::slashdot_menu::fields::Row;
use chrono::TimeZone;
use std::path::Path;
use std::sync::{LazyLock, Mutex};
use std::time::{Duration, Instant, SystemTime};

/// Gate: complete only path-shaped words — contains `/`, or starts
/// with `~` or `.`. Ordinary bare words are inert (the old prompt
/// completer accepted them; the gate keeps prose frames cheap); a
/// dot-word matching nothing shows no dialog. Shared by the sandbox
/// leaf, the prose layer (which pre-gates before the thread hop), and
/// [`FileCompleter`].
pub fn completable(word: &str) -> bool {
    word.contains('/') || word.starts_with('~') || word.starts_with('.')
}

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

/// Split an expanded prefix into `(dir to list, ranking stem)`.
///
/// Ends-in-`/` or an existing dir ⇒ list that dir (empty stem); else
/// list the parent — an **empty parent (bare relative word) ⇒ `.`**,
/// so dot-words like `.gitignore` complete against the cwd (v1 returned
/// nothing here) — with the basename as the stem.
fn split_prefix(expanded: &str, path_is_dir: bool) -> (String, String) {
    if expanded.ends_with('/') || path_is_dir {
        let d = expanded.trim_end_matches('/');
        return (
            if d.is_empty() { "/".to_string() } else { d.to_string() },
            String::new(),
        );
    }
    let path = Path::new(expanded);
    let parent = path
        .parent()
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_default();
    let parent = if parent.is_empty() {
        ".".to_string()
    } else {
        parent
    };
    let stem = path
        .file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default();
    (parent, stem)
}

use std::collections::HashMap;

/// Complete `prefix` against the host FS (`sandbox = false`) or the
/// sandbox-visible FS with host fallback (`sandbox = true`, the v1
/// `complete_path_prefix_sandbox` semantics: sandbox listing wins;
/// while a sandbox is configured but the dir is invisible there,
/// return *nothing* rather than leaking host paths; fall back to the
/// host only when no sandbox is configured at all).
///
/// Candidates are the dir's children (via the L1 listing cache),
/// path-sorted as the ranking input: an empty stem keeps that
/// alphabetical order; a typed stem ranks prefix matches first, then
/// fuzzy matches, dropping non-matches. Empty prefix ⇒ empty (inert).
///
/// Returns (ranked_entries, match_indices) where match_indices maps
/// entry pointer to field_name → match byte indices.
pub fn complete(prefix: &str, sandbox: bool) -> (Vec<Entry>, HashMap<*const Entry, HashMap<String, Vec<usize>>>) {
    if prefix.is_empty() {
        return (Vec::new(), HashMap::new());
    }
    let uses_tilde = prefix.starts_with('~');
    let expanded = crate::path_util::expand_tilde(prefix)
        .to_string_lossy()
        .into_owned();
    let path_is_dir = Path::new(&expanded).is_dir();
    let (dir, stem) = split_prefix(&expanded, path_is_dir);

    let listing = match listing_for(&dir, sandbox) {
        Listing::Found(entries) => entries,
        // Sandbox configured but the dir did not list inside it —
        // never suggest host paths the sandbox cannot see.
        Listing::Invisible => return (Vec::new(), HashMap::new()),
    };

    let mut entries: Vec<Entry> = if uses_tilde {
        let home = std::env::var("HOME").unwrap_or_default();
        listing
            .into_iter()
            .map(|mut e| {
                e.path = tilde_map(&e.path, &home);
                e
            })
            .collect()
    } else {
        listing
    };

    let field_extractors: Vec<(String, Box<dyn Fn(&Entry) -> String>)> = vec![
        ("path".to_string(), Box::new(|e: &Entry| e.path.clone())),
        ("description".to_string(), Box::new(|e: &Entry| e.description())),
    ];
    let (ranked, match_indices) = rank_candidates(
        &entries,
        &stem,
        |e| basename_of(&e.path).to_string(),
        |e| basename_of(&e.path).to_string(),
        &field_extractors,
    );
    entries = ranked.into_iter().cloned().collect();
    (entries, match_indices)
}

/// Prose (in-prompt) FS completer — the prose worker's unit of work
/// (see `doc/slashdot/arg_and_modifier_autocomplete.md` §Prompt-line
/// completion). Always the sandbox-aware mode ([`complete`] with
/// `sandbox = true`: sandbox listing wins, host fallback only when no
/// sandbox is configured). Applies the shared [`completable`] gate;
/// the prose layer additionally pre-gates per keystroke so bare words
/// never reach the worker.
///
/// L2 cache on `(word, listing_stale)`; rows are id = full display
/// path, desc = [`Entry::description`]. Returns `CompletionRows`
/// **by value** (owned, `Send`) so it can run on the worker thread —
/// the caller wraps `Rc::new` after the channel hop. Empty rows = no
/// dialog (gate miss, no match, or every match exact).
///
/// Labels (render path stays label-agnostic): id = `Sandbox path`
/// when a sandbox is configured, else `Host path`; desc =
/// `Description`.
pub struct FileCompleter {
    last: Option<(String, CompletionRows)>,
}

impl FileCompleter {
    pub fn new() -> Self {
        Self { last: None }
    }

    /// Complete `word` against the FS (worker thread; may IO on an L1
    /// miss — the prose layer never calls this on the input loop).
    pub fn complete(&mut self, word: &str) -> CompletionRows {
        let empty = || CompletionRows {
            rows: Vec::new(),
            labels: None,
            match_indices: None,
        };
        if !completable(word) {
            return empty();
        }
        // L2 hit: same word and the L1 listing still fresh.
        if let Some((w, rows)) = &self.last {
            if w == word && !listing_stale(true) {
                return rows.clone();
            }
        }
        let (entries, match_indices_map) = complete(word, true);
        // Exactness: every candidate byte-equal to the word ⇒ nothing
        // left to complete (empty rows ⇒ no dialog; the engine appends
        // the trailing space on Tab).
        let all_exact = !entries.is_empty() && entries.iter().all(|e| e.path == word);
        let rows: Vec<Row> = if all_exact {
            Vec::new()
        } else {
            entries
                .iter()
                .map(|e| Row::new(&e.path).desc(e.description()))
                .collect()
        };

        // Convert match_indices_map from *const Entry to row index
        let match_indices = if !rows.is_empty() {
            let mut result = HashMap::new();
            for (idx, entry) in entries.iter().enumerate() {
                let ptr = entry as *const Entry;
                if let Some(indices) = match_indices_map.get(&ptr) {
                    result.insert(idx, indices.clone());
                }
            }
            Some(result)
        } else {
            None
        };

        let sandbox = crate::tools::sandbox_config().is_some();
        let out = CompletionRows {
            rows,
            labels: Some((
                if sandbox { "Sandbox path" } else { "Host path" }.to_string(),
                "Description".to_string(),
            )),
            match_indices,
        };
        self.last = Some((word.to_string(), out.clone()));
        out
    }
}

/// L1 validity window: sandbox side (a revalidation probe would be a
/// bwrap spawn) and host fallback when the dir stat is unavailable.
const LISTING_TTL: Duration = Duration::from_secs(5);

/// Shared single-slot directory listing cache (L1).
struct ListingSlot {
    dir: String,
    sandbox: bool,
    /// Raw (expanded) paths, path-sorted.
    entries: Vec<Entry>,
    /// Sandbox configured, but the dir did not list inside it.
    invisible: bool,
    fetched_at: Instant,
    /// Host side: the dir's mtime captured *before* listing (the
    /// revalidation probe); `None` for sandbox slots.
    dir_mtime: Option<SystemTime>,
}

static LISTING: LazyLock<Mutex<Option<ListingSlot>>> = LazyLock::new(|| Mutex::new(None));

/// Drop the shared L1 listing slot. Called when the sandbox config is
/// re-published (`tools::set_sandbox`) — the invisible/visible state
/// of every dir can change with it.
pub fn invalidate_listing() {
    if let Ok(mut guard) = LISTING.lock() {
        *guard = None;
    }
}

/// Would a [`complete`] call in this mode refetch its listing right
/// now (slot missing, mode mismatch, or failed revalidation)?
/// Completion leaves use this to invalidate their row cache without
/// performing the IO themselves.
pub fn listing_stale(sandbox: bool) -> bool {
    let Ok(guard) = LISTING.lock() else {
        return true;
    };
    match guard.as_ref() {
        None => true,
        Some(slot) => slot.sandbox != sandbox || !slot_fresh(slot),
    }
}

/// Host: revalidate with one dir-`mtime` stat (stat unavailable ⇒
/// TTL). Sandbox: TTL only.
fn slot_fresh(slot: &ListingSlot) -> bool {
    if slot.sandbox {
        return slot.fetched_at.elapsed() < LISTING_TTL;
    }
    match std::fs::metadata(&slot.dir).and_then(|m| m.modified()) {
        Ok(now) if slot.dir_mtime.is_some() => Some(now) == slot.dir_mtime,
        _ => slot.fetched_at.elapsed() < LISTING_TTL,
    }
}

enum Listing {
    Found(Vec<Entry>),
    /// Sandbox configured, but the dir is not visible inside it.
    Invisible,
}

/// Cached listing for `dir` (fetching + replacing the slot on miss or
/// staleness; the lock is held across the fetch, so a slow bwrap spawn
/// can't stampede).
fn listing_for(dir: &str, sandbox: bool) -> Listing {
    let Ok(mut guard) = LISTING.lock() else {
        return fetch_listing(dir, sandbox);
    };
    if let Some(slot) = guard.as_ref() {
        if slot.dir == dir && slot.sandbox == sandbox && slot_fresh(slot) {
            return if slot.invisible {
                Listing::Invisible
            } else {
                Listing::Found(slot.entries.clone())
            };
        }
    }
    let dir_mtime = if sandbox {
        None
    } else {
        // Captured before listing: a change after the stat makes the
        // stored mtime stale → the next call refetches.
        std::fs::metadata(dir)
            .and_then(|m| m.modified())
            .ok()
    };
    let listing = fetch_listing(dir, sandbox);
    let (entries, invisible) = match &listing {
        Listing::Found(entries) => (entries.clone(), false),
        Listing::Invisible => (Vec::new(), true),
    };
    *guard = Some(ListingSlot {
        dir: dir.to_string(),
        sandbox,
        entries,
        invisible,
        fetched_at: Instant::now(),
        dir_mtime,
    });
    listing
}

/// Uncached fetch: sandbox listing wins; configured-but-invisible ⇒
/// `Invisible`; no sandbox ⇒ host fallback.
fn fetch_listing(dir: &str, sandbox: bool) -> Listing {
    if sandbox {
        return match list_sandbox(dir) {
            Some(entries) => Listing::Found(sorted(entries)),
            None if crate::tools::sandbox_config().is_some() => Listing::Invisible,
            None => Listing::Found(sorted(list_host(dir))),
        };
    }
    Listing::Found(sorted(list_host(dir)))
}

/// Path-sorted — the ranking input (empty query ⇒ alphabetical).
fn sorted(mut entries: Vec<Entry>) -> Vec<Entry> {
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
    fn split_prefix_empty_parent_is_dot() {
        // v1 returned nothing for bare relative words (read_dir("") fails);
        // the dot gate makes them reachable, so the parent must be ".".
        assert_eq!(
            split_prefix(".gitignore", false),
            (".".to_string(), ".gitignore".to_string())
        );
        assert_eq!(split_prefix("foo", false), (".".to_string(), "foo".to_string()));
        assert_eq!(split_prefix("x/y", false), ("x".to_string(), "y".to_string()));
        assert_eq!(split_prefix("/a/b/", true), ("/a/b".to_string(), String::new()));
        assert_eq!(split_prefix("/", true), ("/".to_string(), String::new()));
        assert_eq!(split_prefix("/a/b", true), ("/a/b".to_string(), String::new()));
    }

    #[test]
    #[serial_test::serial]
    fn empty_prefix_is_inert() {
        assert!(complete("", false).0.is_empty());
    }

    #[test]
    #[serial_test::serial]
    fn host_dir_listing_full_paths_sorted_with_metadata() {
        let dir = tempfile::tempdir().unwrap();
        write(&dir.path().join("a.txt"), "hello"); // 5 bytes
        fs::create_dir(dir.path().join("sub")).unwrap();
        let prefix = format!("{}/", dir.path().display());

        let (entries, _) = complete(&prefix, false);
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
        let (stem, _) = complete(&format!("{}/a", dir.path().display()), false);
        assert_eq!(stem.len(), 1, "{stem:?}");
        assert!(stem[0].path.ends_with("a.txt"), "{stem:?}");
    }

    #[test]
    #[serial_test::serial]
    fn host_existing_dir_without_slash_lists_children() {
        // v1 quirk preserved: an existing dir prefix lists its contents
        // even without a trailing slash.
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir(dir.path().join("sub")).unwrap();
        write(&dir.path().join("sub/x"), "y");
        let (entries, _) = complete(&format!("{}/sub", dir.path().display()), false);
        assert_eq!(entries.len(), 1, "{entries:?}");
        assert!(entries[0].path.ends_with("sub/x"));
    }

    #[test]
    #[serial_test::serial]
    fn stem_ranking_prefix_then_fuzzy() {
        let dir = tempfile::tempdir().unwrap();
        write(&dir.path().join("abcde"), "1");
        write(&dir.path().join("someabc"), "2");
        write(&dir.path().join("zzz"), "3");

        let (got, _) = complete(&format!("{}/abc", dir.path().display()), false);
        let names: Vec<&str> = got.iter().map(|e| basename_of(&e.path)).collect();
        // Prefix tier first, fuzzy second, non-match dropped — the whole
        // dir is ranked (v1's strict starts_with would drop someabc).
        assert_eq!(names, vec!["abcde", "someabc"], "{names:?}");
    }

    #[test]
    #[serial_test::serial]
    fn dir_level_listing_is_alphabetical() {
        let dir = tempfile::tempdir().unwrap();
        write(&dir.path().join("zeta"), "");
        write(&dir.path().join("alpha"), "");
        write(&dir.path().join("mid"), "");

        let (got, _) = complete(&format!("{}/", dir.path().display()), false);
        let names: Vec<&str> = got.iter().map(|e| basename_of(&e.path)).collect();
        // Empty stem ⇒ rank_candidates' input order ⇒ path-sorted.
        assert_eq!(names, vec!["alpha", "mid", "zeta"], "{names:?}");
    }

    #[test]
    #[serial_test::serial]
    fn host_listing_cache_revalidates_on_change() {
        let dir = tempfile::tempdir().unwrap();
        write(&dir.path().join("first"), "");
        let (e1, _) = complete(&format!("{}/", dir.path().display()), false);
        assert!(e1.iter().any(|e| e.path.ends_with("/first")));

        write(&dir.path().join("second"), "");
        let (e2, _) = complete(&format!("{}/", dir.path().display()), false);
        assert!(
            e2.iter().any(|e| e.path.ends_with("/second")),
            "dir-mtime revalidation must refetch: {e2:?}"
        );
    }

    #[test]
    #[serial_test::serial]
    fn listing_stale_tracks_slot_mode() {
        let dir = tempfile::tempdir().unwrap();
        write(&dir.path().join("x"), "");
        let _ = complete(&format!("{}/", dir.path().display()), false);
        assert!(!listing_stale(false), "fresh host slot");
        assert!(listing_stale(true), "mode mismatch is stale");
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
        let (entries, _) = complete(&format!("{}/", dir.path().display()), true);
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
        let (entries, _) = complete(&format!("{}/", dir.path().display()), true);
        assert!(entries.is_empty(), "must not leak host paths: {entries:?}");
        crate::tools::set_sandbox(None);
    }

    // --- completable gate (shared: sandbox leaf + prose layer) ---

    #[test]
    fn completable_gate_path_shaped_words_only() {
        // Bare words inert — the accepted prose regression (doc:
        // "Bare relative words (`cat fo`) stay inert; `./` triggers").
        assert!(!completable("cat"));
        assert!(!completable("fo"));
        assert!(completable("./fo"));
        assert!(completable("a/b"));
        assert!(completable("~"));
        assert!(completable("~/x"));
        assert!(completable(".gitignore"));
        assert!(completable(".."));
    }

    // --- FileCompleter (prose worker unit of work) ---

    #[test]
    #[serial_test::serial]
    fn file_completer_gate_miss_is_inert() {
        crate::tools::set_sandbox(None);
        let mut fc = FileCompleter::new();
        let out = fc.complete("just some prose");
        assert!(out.rows.is_empty());
        assert!(out.labels.is_none(), "gate miss never reaches rows");
    }

    #[test]
    #[serial_test::serial]
    fn file_completer_rows_full_paths_and_host_labels() {
        crate::tools::set_sandbox(None);
        let dir = tempfile::tempdir().unwrap();
        write(&dir.path().join("alpha"), "1");
        fs::create_dir(dir.path().join("sub")).unwrap();
        let prefix = format!("{}/al", dir.path().display());

        let mut fc = FileCompleter::new();
        let out = fc.complete(&prefix);
        assert_eq!(out.rows.len(), 1, "{out:?}");
        assert!(out.rows[0].id.ends_with("/alpha"), "{out:?}");
        assert_eq!(
            out.labels,
            Some(("Host path".to_string(), "Description".to_string()))
        );
    }

    #[test]
    #[serial_test::serial]
    fn file_completer_exact_word_yields_empty() {
        crate::tools::set_sandbox(None);
        let dir = tempfile::tempdir().unwrap();
        write(&dir.path().join("only"), "x");
        let full = format!("{}/only", dir.path().display());

        let mut fc = FileCompleter::new();
        let out = fc.complete(&full);
        assert!(out.rows.is_empty(), "exact match = nothing to complete: {out:?}");
    }

    #[test]
    #[serial_test::serial]
    fn file_completer_l2_serves_cached_rows_within_ttl() {
        crate::tools::set_sandbox(None);
        let dir = tempfile::tempdir().unwrap();
        write(&dir.path().join("first"), "1");
        let prefix = format!("{}/", dir.path().display());

        let mut fc = FileCompleter::new();
        let rows1 = fc.complete(&prefix);
        assert!(rows1.rows.iter().any(|r| r.id.ends_with("/first")));

        // New file: the L1 slot is sandbox-mode (TTL, no mtime check),
        // so a same-word L2 hit must NOT refetch — new file invisible.
        write(&dir.path().join("second"), "2");
        let rows2 = fc.complete(&prefix);
        assert_eq!(rows1.rows.len(), rows2.rows.len(), "L2 must serve cache");
        assert!(!rows2.rows.iter().any(|r| r.id.ends_with("/second")));

        // Invalidate the L1 → next call refetches and sees the new file.
        invalidate_listing();
        let rows3 = fc.complete(&prefix);
        assert!(rows3.rows.iter().any(|r| r.id.ends_with("/second")));
    }

    #[test]
    #[serial_test::serial]
    fn file_completer_sandbox_label_when_configured() {
        crate::tools::set_sandbox(Some(crate::config::SandboxConfig::default()));
        let dir = tempfile::tempdir().unwrap();
        write(&dir.path().join("f"), "x");
        let mut fc = FileCompleter::new();
        // Tempdir invisible under the default sandbox ⇒ rows empty, but
        // the label choice is visible on the response.
        let out = fc.complete(&format!("{}/", dir.path().display()));
        assert_eq!(
            out.labels,
            Some(("Sandbox path".to_string(), "Description".to_string()))
        );
        crate::tools::set_sandbox(None);
    }
}
