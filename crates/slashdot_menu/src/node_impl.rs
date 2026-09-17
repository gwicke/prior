//! Node implementation trait (v2.5 design §2).
//!
//! Host applications implement `SlashDotNode` for each menu command.
//! The engine handles all traversal, cursor, and child management internally.

use crate::fields::Row;
use crate::render::RenderPlan;
use crate::value::Value;

// --- Public types used by SlashDotNode implementors ---

/// Path arguments parsed from the prompt by the generic engine (§2).
pub struct PathArgs {
    /// Resolved node id path so far (e.g. `["tree", "compact"]`).
    pub ids: Vec<String>,
    /// Token range in the current level (terms/modifiers/selection/args).
    pub suffix: std::ops::Range<usize>,
    /// Parsed arguments from the selection + modifiers, if any.
    pub args: Option<Args>,
}

/// Parsed arguments from the selection and modifiers at a level.
pub struct Args {
    /// Modifier values keyed by name (e.g. `("prompt", "some text")`).
    pub modifiers: Vec<(String, Value)>,
    /// Positional tokens (non-modifier, non-selection words).
    pub positional: Vec<String>,
}

/// The rows a node contributes to its parent's listing.
pub struct DataResponse {
    pub rows: Vec<Row>,
    /// Ids of rows that are "currently active" (`*` marker, §5.1).
    pub active_ids: Vec<String>,
}

/// Error returned by `data()` when the node cannot produce rows.
pub struct DataError {
    pub message: String,
}

/// Result type for `data()`.
pub type DataResult = Result<std::rc::Rc<DataResponse>, DataError>;

/// Outcome of `submit()` / `prepare()`.
pub enum SubmitResult {
    /// Action completed; close the menu. Optional message.
    Done(Option<String>),
    /// Prep step done; engine shows confirm dialog. Subsequent Enter
    /// calls `submit()` again.
    Prepared,
    /// Rewrite the buffer to a new menu path (e.g. `/compact` →
    /// `/tree <id> compact `).
    NewPath(String),
}

/// Outcome of `node_traverse()` — what the host should do next.
pub enum MenuResult {
    /// Keep interacting. The host renders the plan and sets the prompt.
    Continue(RenderPlan, String),
    /// Menu done; close the overlay. Optional message to print.
    DoneSuccess(Option<String>),
    /// Error; close the overlay and show the message.
    DoneError(String),
}

/// Simplified key events the host sends to the engine.
pub enum HostKey {
    Tab,
    Enter,
    Esc,
    /// Move cursor by delta (+1 for down, -1 for up, ±N for pgup/pgdn).
    MoveCursor(i32),
}

// --- Public trait for host implementations ---

/// Host implementations of menu nodes. Each command is one `impl SlashDotNode`.
/// The engine handles all traversal, cursor, and child management internally.
///
/// All methods have default implementations (no-op / empty). Override only
/// what the node needs.
pub trait SlashDotNode {
    /// The rows this node contributes to its parent's listing.
    /// Return `Ok(Rc<DataResponse>)` with rows + active ids; `Err` to
    /// report an error. Default: empty response.
    fn data(&mut self, _epoch: u64, _path_args: &PathArgs) -> DataResult {
        Ok(std::rc::Rc::new(DataResponse {
            rows: Vec::new(),
            active_ids: Vec::new(),
        }))
    }

    /// The leaf accept behavior. Called on Enter when this node is the
    /// selected leaf. Default: `Done(None)`.
    fn submit(&mut self, _epoch: u64, _path_args: &PathArgs) -> SubmitResult {
        SubmitResult::Done(None)
    }

    /// Optional confirm hook (§7 step 7a). Default: `self.submit()`.
    /// An override runs the prep phase (may open an external editor via
    /// a callback captured at construction), stages the payload in
    /// private state, and returns `SubmitResult::Prepared`.
    fn prepare(&mut self, epoch: u64, path_args: &PathArgs) -> SubmitResult {
        self.submit(epoch, path_args)
    }

    /// Non-selectable header lines (replaces v2 DetailProvider).
    /// Default: empty. The engine truncates based on terminal size.
    fn header(&mut self, _path_args: &PathArgs) -> Vec<String> {
        Vec::new()
    }
}

// --- Defaults ---