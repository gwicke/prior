//! A per-test directory for the diagnostic byte-stream dumps.
//!
//! The pty tests capture the raw bytes `prior` emits and render them through
//! a terminal emulator. When one of their assertions fails, that byte stream is
//! the only record of what the program actually drew — the screen the emulator
//! shows has already lost the escape sequences and the ordering that explain
//! *why* it looks wrong. So the stream gets written out before the assertions
//! run, and the path has to satisfy two things:
//!
//! * **Unique per run.** `/tmp` is shared by every checkout on the machine. A
//!   fixed name has concurrent runs overwriting each other — and inside a
//!   single test binary, parallel test threads doing the same, so the dump left
//!   behind would describe somebody else's failure. The name therefore carries
//!   the pid and a timestamp.
//! * **Kept on failure, removed on success.** The dumps are worth keeping
//!   exactly when the test did not pass, and are litter in `/tmp` otherwise.
//!   [`Dumps`] decides which by asking, on the way out, whether the thread is
//!   unwinding.
//!
//! The guard is taken at the top of the test body and therefore lives until the
//! body returns — long enough to survive every assertion that could fail. A
//! dump written from a helper that returns before the assertions run would be
//! deleted before it could ever be read.

use std::path::PathBuf;

/// A directory holding one test's diagnostic dumps.
///
/// Dropped at the end of the test body: the directory is removed on the way out
/// unless the test is unwinding, in which case its path is printed and the
/// files are left in place.
pub struct Dumps {
    path: PathBuf,
}

impl Dumps {
    /// Create the dump directory for a test. `tag` names the test within it.
    pub fn new(tag: &str) -> Dumps {
        // Keep the tag to characters safe in a single path segment: it comes
        // from a literal at the call site, but a stray `/` would turn a
        // directory name into a missing parent.
        let tag: String = tag
            .chars()
            .map(|c| {
                if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                    c
                } else {
                    '-'
                }
            })
            .collect();
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        let name = format!("prior-dumps-{}-{tag}-{nanos}", std::process::id());
        let path = std::env::temp_dir().join(name);
        std::fs::create_dir_all(&path).expect("create dump dir");
        eprintln!("[dump] {}", path.display());
        Dumps { path }
    }

    /// Write one dump — conventionally the whole pty byte stream, which is what
    /// the failure message points the reader at.
    pub fn write(&self, name: &str, data: &[u8]) {
        std::fs::write(self.path.join(name), data).expect("write dump");
    }
}

impl Drop for Dumps {
    fn drop(&mut self) {
        if std::thread::panicking() {
            eprintln!(
                "[dump] test failed; dumps kept for diagnosis in {}",
                self.path.display()
            );
            return;
        }
        if let Err(e) = std::fs::remove_dir_all(&self.path) {
            eprintln!("[dump] could not remove {}: {e}", self.path.display());
        }
    }
}
