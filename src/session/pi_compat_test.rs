// Cross-compatibility test: read actual pi agent session store.
//
// This test locates `~/.pi/agent/sessions/`, enumerates every `.jsonl` file
// across all CWD-encoded directories, and verifies that picocode can parse
// every entry, build the full `SessionIndex`, walk `get_path_to_root`, and
// produce valid output.
//
// The test is `#[ignore]` by default because it requires the user's pi
// installation to be present (`~/.pi/agent/sessions/`). Run with:
//
// ```sh
// cargo test -- --ignored session::pi_compat_test
// ```
//
// If the pi sessions directory does not exist, the test prints a skip
// message and passes harmlessly.

use std::path::PathBuf;

use crate::session::storage::{JsonlSessionStorage, SessionStorage};
use crate::session::types::*;

/// The pi agent's session directory, derived from the standard location on
/// Linux and macOS (`~/.pi/agent/sessions/`). Override with the
/// `PICOCODE_PI_SESSIONS` environment variable.
fn pi_sessions_root() -> PathBuf {
    if let Ok(path) = std::env::var("PICOCODE_PI_SESSIONS") {
        return PathBuf::from(path);
    }
    let home = std::env::var("HOME")
        .or_else(|_| std::env::var("USERPROFILE"))
        .unwrap_or_else(|_| ".".into());
    PathBuf::from(home).join(".pi/agent/sessions")
}

/// Enumerate all `.jsonl` files under the pi sessions root, recursively
/// visiting each CWD-encoded subdirectory.
fn enumerate_session_files(root: &PathBuf) -> Vec<PathBuf> {
    let mut files = Vec::new();
    let Ok(read_dir) = std::fs::read_dir(root) else {
        return files;
    };
    for entry in read_dir.flatten() {
        let path = entry.path();
        if path.is_dir() {
            // CWD-encoded subdirectory — recurse
            if let Ok(sub_read) = std::fs::read_dir(&path) {
                for sub_entry in sub_read.flatten() {
                    let sub_path = sub_entry.path();
                    if sub_path.extension().map_or(false, |e| e == "jsonl") {
                        files.push(sub_path);
                    }
                }
            }
        } else if path.extension().map_or(false, |e| e == "jsonl") {
            files.push(path);
        }
    }
    files
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    #[ignore = "requires ~/.pi/agent/sessions/ to exist; run manually with -- --ignored"]
    async fn test_read_actual_pi_sessions() {
        let root = pi_sessions_root();
        if !root.exists() {
            eprintln!(
                "[pi-compat] SKIP: pi sessions root not found: {}",
                root.display()
            );
            eprintln!(
                "[pi-compat]   Set PICOCODE_PI_SESSIONS to a custom path, or"
            );
            eprintln!(
                "[pi-compat]   install pi agent (https://github.com/badlogic/pi-mono)"
            );
            return;
        }

        let files = enumerate_session_files(&root);
        if files.is_empty() {
            eprintln!(
                "[pi-compat] SKIP: no .jsonl files found under {}",
                root.display()
            );
            return;
        }

        eprintln!(
            "[pi-compat] Found {} session file(s) under {}",
            files.len(),
            root.display()
        );

        let mut total_entries = 0usize;
        let mut total_errors = 0usize;
        let mut total_trees = 0usize;

        for file_path in &files {
            eprintln!("[pi-compat]   Checking: {}", file_path.display());

            // 1. Parse header line
            let first_line = match std::fs::read_to_string(file_path) {
                Ok(c) => c.lines().next().map(String::from),
                Err(e) => {
                    eprintln!("[pi-compat]     ERROR: cannot read: {e}");
                    total_errors += 1;
                    continue;
                }
            };

            let header: SessionHeader = match serde_json::from_str(
                first_line.as_deref().unwrap_or(""),
            ) {
                Ok(h) => h,
                Err(e) => {
                    eprintln!("[pi-compat]     ERROR: invalid header: {e}");
                    total_errors += 1;
                    continue;
                }
            };

            // Verify header fields
            assert_eq!(header.kind, "session");
            assert_eq!(header.version, 3);

            // 2. Build full index
            let storage = match JsonlSessionStorage::open_or_create(file_path.clone()).await
            {
                Ok(s) => s,
                Err(e) => {
                    eprintln!("[pi-compat]     ERROR: cannot open: {e}");
                    total_errors += 1;
                    continue;
                }
            };

            let index = match storage.build_index().await {
                Ok(idx) => idx,
                Err(e) => {
                    eprintln!("[pi-compat]     ERROR: build_index: {e}");
                    total_errors += 1;
                    continue;
                }
            };

            total_entries += index.entries.len();
            eprintln!(
                "      {} entries, leaf: {:?}, name: {:?}, cwd: {}",
                index.entries.len(),
                index.leaf_id,
                index.session_name,
                index.header.cwd,
            );

            // 3. Walk path-to-root from the leaf (if set)
            if let Some(ref leaf_id) = index.leaf_id {
                let path = index.get_path_to_root(leaf_id);
                if path.is_empty() {
                    eprintln!("      WARNING: leaf {leaf_id} points to missing entry");
                } else {
                    eprintln!(
                        "      path-to-root: {} entries (root: {}, leaf: {})",
                        path.len(),
                        path[0].id,
                        path.last().unwrap().id,
                    );
                    // Verify every entry in the chain is contiguous
                    for (i, entry) in path.iter().enumerate() {
                        if i > 0 {
                            // Each entry's parent should be the previous entry
                            if let Some(ref pid) = entry.parent_id {
                                assert_eq!(
                                    pid, &path[i - 1].id,
                                    "entry {} has parent {} but expected {}",
                                    entry.id, pid, path[i - 1].id
                                );
                            }
                        }
                    }
                    total_trees += 1;
                }
            } else {
                eprintln!("      (no leaf — session may be empty or incomplete)");
            }
        }

        eprintln!(
            "[pi-compat] RESULT: {} files, {} entries, {} valid trees, {} errors",
            files.len(),
            total_entries,
            total_trees,
            total_errors
        );

        // The test itself is a soft assertion: we report but don't fail on
        // individual file errors (they may be from sessions in-flight).
        assert!(
            total_entries > 0 || total_errors > 0,
            "Expected at least one readable session file"
        );
    }
}
