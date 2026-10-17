//! The `--log-messages` JSONL debug log.
//!
//! One JSON object per line, capturing raw rig [`Message`]s as sent to and
//! received from the model. Debugging aid only — it writes nothing unless
//! `--log-messages <file>` named a path, and every failure is reported to
//! stderr and swallowed so logging never breaks the conversation.
//!
//! Nothing here knows about the agent. [`MessageLogger`] is handed to the
//! hook as an `Arc<Option<..>>`-shaped capability and told which turn each
//! message belongs to; the turn id is what makes a round trip readable after
//! the fact, since each completion call re-sends the whole history.

use chrono::Utc;
use rig_core::message::Message;

use crate::Result;

/// Human-readable role label for a rig [`Message`], used in the JSONL log.
pub(crate) fn message_role(msg: &Message) -> &'static str {
    match msg {
        Message::System { .. } => "system",
        Message::User { .. } => "user",
        Message::Assistant { .. } => "assistant",
    }
}

/// JSONL message logger for debugging LLM interactions.
///
/// Writes one JSON object per line capturing a raw rig [`Message`] (as sent to
/// or received from the model) with timestamp, direction, role, and turn id.
/// The file is created lazily on the first write; parent directories are
/// created as needed. Enabled with `--log-messages <file>`.
pub struct MessageLogger {
    file: tokio::sync::Mutex<Option<tokio::fs::File>>,
    path: std::path::PathBuf,
    turn_counter: std::sync::atomic::AtomicU64,
    provider: String,
    model: String,
}

impl MessageLogger {
    /// Create a new MessageLogger that writes to the specified path.
    /// The file is created lazily on first write.
    pub fn new(path: String, provider: String, model: String) -> Self {
        Self {
            file: tokio::sync::Mutex::new(None),
            path: std::path::PathBuf::from(path),
            turn_counter: std::sync::atomic::AtomicU64::new(0),
            provider,
            model,
        }
    }

    /// Advance to the next completion call, returning its 1-based turn id.
    ///
    /// Every message logged under the same turn id belongs to one
    /// request/response round trip: the full context sent (history + prompt)
    /// and the assistant content received in reply.
    pub fn advance_turn(&self) -> u64 {
        self.turn_counter
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst)
            + 1
    }

    /// Current turn id (without incrementing) — used by the response side to
    /// tag the reply with the same turn id as the request that produced it.
    pub fn current_turn_id(&self) -> u64 {
        self.turn_counter.load(std::sync::atomic::Ordering::SeqCst)
    }

    /// Log a raw rig `Message` as one JSONL line.
    ///
    /// Failures (unwritable path, disk full) are reported to stderr and
    /// swallowed so message logging never breaks the conversation.
    pub async fn log_message(&self, direction: &str, message: &Message, turn_id: u64) {
        let entry = serde_json::json!({
            "timestamp": Utc::now().to_rfc3339(),
            "direction": direction,
            "role": message_role(message),
            "message": message,
            "turn_id": turn_id,
            "provider": self.provider,
            "model": self.model,
        });

        if let Err(e) = self.write_line(&entry).await {
            // A process diagnostic, not content: `MessageLogger` predates and
            // holds no `Content` handle, so there is no kind to ride (design
            // §TermIO: stderr is the diagnostic sink).
            eprintln!("Warning: failed to write to message log: {}", e);
        }
    }

    /// Ensure the file handle is open, creating the file if needed.
    async fn ensure_file_open(
        &self,
    ) -> Result<tokio::sync::MutexGuard<'_, Option<tokio::fs::File>>> {
        let mut guard = self.file.lock().await;
        if guard.is_none() {
            // Create parent directories
            if let Some(parent) = self.path.parent() {
                tokio::fs::create_dir_all(parent).await?;
            }
            // Open (or create) the file for appending
            let file = tokio::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(&self.path)
                .await?;
            *guard = Some(file);
        }
        Ok(guard)
    }

    /// Write a JSON line to the file.
    async fn write_line(&self, entry: &serde_json::Value) -> Result<()> {
        let guard = self.ensure_file_open().await?;
        if let Some(file) = guard.as_ref() {
            let line = serde_json::to_string(entry)?;
            let mut file = file.try_clone().await?;
            use tokio::io::AsyncWriteExt;
            file.write_all(line.as_bytes()).await?;
            file.write_all(b"\n").await?;
        }
        Ok(())
    }
}
