//! Event-driven watch of `settings.toml` (doc/settings_reload.md).
//!
//! One [`notify`] watcher on the settings file's *parent directory*,
//! non-recursive, filtered in the callback to events naming the file. Watching
//! the directory rather than the file is deliberate: every editor save pattern
//! (truncate+rewrite, temp-file+rename, delete+recreate) emits an event naming
//! the file inside a directory watch, while a file watch dies with the inode
//! on rename.
//!
//! The watcher's thread sets a flag and pokes a `tokio::sync::Notify` per
//! matching event; the REPL's key wait selects on the notify, so an external
//! edit wakes an idle prompt the same way a prose-completion deadline does.
//! The flag, not the notify, is the source of truth: a poke may be coalesced
//! (notify semantics), but `take_pending` reports "an event arrived since the
//! last check" regardless, so no wake can be lost. Bursts therefore collapse
//! into one reload — and a reload is idempotent, so over-signalling costs one
//! extra re-read, never a wrong state.
//!
//! Only *content* events count ([`is_content_change`]): an access event is
//! dropped, because the reload's own `read` raises one and counting it would
//! make each reload trigger the next.
//!
//! Failure to arm (no parent directory, no inotify budget) degrades to `None`:
//! the caller simply never wakes, which is today's restart-to-see-edits
//! behaviour. Nothing else changes.

use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use notify::{event::EventKind, RecursiveMode, Watcher};

/// Whether an event kind can mean "the settings file's contents changed".
///
/// The one thing that must be excluded is [`EventKind::Access`]: `inotify`
/// reports an `Open` for every `read`, and the reload itself reads the file —
/// so treating an access as a change makes each reload trigger the next one,
/// forever (this was a live lock-up, not a theoretical one). A read never
/// changes contents; only a write, create, rename or delete does.
fn is_content_change(kind: &EventKind) -> bool {
    !matches!(kind, EventKind::Access(_))
}

/// Armed watch of one settings file. Owns the `notify` watcher (keep-alive —
/// dropping it unregisters the watch) and the change flag it feeds.
pub struct SettingsWatch {
    /// Dropping this unregisters the directory watch.
    _watcher: notify::RecommendedWatcher,
    /// Set by the watcher thread on every matching event; cleared by
    /// [`take_pending`](Self::take_pending). The durable half of the signal —
    /// survives notify coalescing, so a wake is never lost.
    pending: Arc<AtomicBool>,
    /// Wakes the REPL's key wait. Pokes may coalesce; that is fine because
    /// `pending` carries the count-free fact of "something changed".
    notify: Arc<tokio::sync::Notify>,
}

impl SettingsWatch {
    /// Watch `settings_path`'s parent directory for events naming the file.
    /// `None` when the watch cannot be armed (missing parent dir, watcher
    /// backend unavailable, watch limit reached) — callers degrade to "no
    /// live reload", never to an error.
    pub fn spawn(settings_path: &Path) -> Option<SettingsWatch> {
        let dir = settings_path.parent()?;
        let file_name = settings_path.file_name()?.to_os_string();
        let pending = Arc::new(AtomicBool::new(false));
        let notify = Arc::new(tokio::sync::Notify::new());
        let (p, n) = (Arc::clone(&pending), Arc::clone(&notify));
        let mut watcher = notify::recommended_watcher(move |res: notify::Result<notify::Event>| {
            let Ok(event) = res else { return };
            if is_content_change(&event.kind)
                && event
                    .paths
                    .iter()
                    .any(|p| p.file_name() == Some(&file_name))
            {
                p.store(true, Ordering::SeqCst);
                n.notify_one();
            }
        })
        .ok()?;
        // Non-recursive: siblings (`sessions/`, `token/`, …) are irrelevant,
        // and the filter above already rejects anything not named like the
        // settings file.
        watcher.watch(dir, RecursiveMode::NonRecursive).ok()?;
        Some(SettingsWatch {
            _watcher: watcher,
            pending,
            notify,
        })
    }

    /// Wait for the next change signal. Returns only on a signal; a dead
    /// watcher (dropped sender side) parks forever rather than spinning the
    /// REPL — with no watcher there is nothing to wake for.
    pub async fn notified(&self) {
        self.notify.notified().await;
    }

    /// True when at least one event arrived since the last call; clears the
    /// flag so the burst counts once. Call sites run this *before* acting on
    /// a key (so a keypress that lands in the same wake as an external edit
    /// always reads the reloaded store) and on the wake itself (before the
    /// next repaint).
    pub fn take_pending(&self) -> bool {
        self.pending.swap(false, Ordering::SeqCst)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    /// The armed watch sees an external write to the file, and `take_pending`
    /// reports it. The generous deadline absorbs CI scheduling noise; inotify
    /// delivers in microseconds locally.
    #[tokio::test]
    async fn external_write_wakes_the_watch() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("settings.toml");
        std::fs::write(&path, "tool_call_limit = 50\n").unwrap();
        let watch = SettingsWatch::spawn(&path).expect("watch arms on an existing dir");

        // Give the watcher a moment to register the directory watch.
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        let mut f = std::fs::OpenOptions::new().write(true).open(&path).unwrap();
        f.write_all(b"tool_call_limit = 51\n").unwrap();
        f.sync_all().unwrap();

        tokio::time::timeout(std::time::Duration::from_secs(5), watch.notified())
            .await
            .expect("the write wakes the watch");
        assert!(watch.take_pending());
    }

    /// The flag outlives the notify poke: even if the wake was consumed (or
    /// coalesced away) the fact of the change is still reportable — this is
    /// the no-lost-wake guarantee the REPL's select relies on.
    #[tokio::test]
    async fn take_pending_reports_without_a_wait() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("settings.toml");
        std::fs::write(&path, "x = 1\n").unwrap();
        let watch = SettingsWatch::spawn(&path).expect("watch arms");

        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        std::fs::write(&path, "x = 2\n").unwrap();
        // Wait for delivery via the flag itself (poll), not the notify.
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(5);
        while !watch.take_pending() {
            assert!(
                tokio::time::Instant::now() < deadline,
                "the event must become pending"
            );
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        // Cleared: a second check is false until the next event.
        assert!(!watch.take_pending());
    }

    /// A file that does not match the watched name never wakes the watch —
    /// the `sessions/` sibling traffic must not trigger reloads.
    #[tokio::test]
    async fn sibling_writes_do_not_wake() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("settings.toml");
        std::fs::write(&path, "tool_call_limit = 50\n").unwrap();
        let watch = SettingsWatch::spawn(&path).expect("watch arms");

        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        std::fs::write(tmp.path().join("recent_models.txt"), "a\n").unwrap();
        // Short window: the sibling write would have been delivered by now if
        // it were going to be.
        let woke =
            tokio::time::timeout(std::time::Duration::from_millis(200), watch.notified()).await;
        assert!(woke.is_err(), "a sibling file must not wake the watch");
        assert!(!watch.take_pending());
    }

    /// A burst coalesces into one pending report: the flag is set, not
    /// counted, so five writes wake once and drain to false.
    #[tokio::test]
    async fn bursts_coalesce() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("settings.toml");
        std::fs::write(&path, "x = 1\n").unwrap();
        let watch = SettingsWatch::spawn(&path).expect("watch arms");

        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        for i in 0..5 {
            std::fs::write(&path, format!("x = {}\n", i + 2)).unwrap();
        }
        tokio::time::timeout(std::time::Duration::from_secs(5), watch.notified())
            .await
            .expect("the burst wakes the watch");
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        assert!(watch.take_pending(), "the burst was signalled");
        assert!(!watch.take_pending(), "drained: nothing left pending");
    }

    /// Reading the file is not a change. This is the self-trigger regression:
    /// the reload reads the file it was woken for, inotify reports that read as
    /// `Access(Open)`, and counting it made every reload schedule the next one —
    /// an endless reload loop that starved the REPL (it never repainted, so an
    /// integration test saw only the pre-edit screen).
    #[tokio::test]
    async fn reading_the_file_does_not_wake() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("settings.toml");
        std::fs::write(&path, "x = 1\n").unwrap();
        // Arm *after* the write above so only the reads below are in scope.
        let watch = SettingsWatch::spawn(&path).expect("watch arms");
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        for _ in 0..5 {
            std::fs::read_to_string(&path).unwrap();
        }
        let woke =
            tokio::time::timeout(std::time::Duration::from_millis(300), watch.notified()).await;
        assert!(woke.is_err(), "a read must not wake the watch");
        assert!(!watch.take_pending(), "a read must not be pending");
    }

    /// The kind filter itself: access excluded, everything else a change.
    #[test]
    fn access_events_are_not_content_changes() {
        use notify::event::{AccessKind, AccessMode, CreateKind, ModifyKind, RemoveKind};
        assert!(!is_content_change(&EventKind::Access(AccessKind::Open(
            AccessMode::Any
        ))));
        assert!(!is_content_change(&EventKind::Access(AccessKind::Read)));
        assert!(!is_content_change(&EventKind::Access(AccessKind::Close(
            AccessMode::Write
        ))));
        assert!(is_content_change(&EventKind::Create(CreateKind::File)));
        assert!(is_content_change(&EventKind::Modify(ModifyKind::Data(
            notify::event::DataChange::Content
        ))));
        assert!(is_content_change(&EventKind::Remove(RemoveKind::File)));
    }
}
