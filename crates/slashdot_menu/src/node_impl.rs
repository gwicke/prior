//! Node implementation trait (v2.5 design §2).
//!
//! Host applications implement `SlashDotNode` for each menu command.
//! The engine handles all traversal, cursor, and child management internally.

use crate::fields::Row;
use crate::render::RenderPlan;

// --- Public types used by SlashDotNode implementors ---

/// Path arguments constructed by the engine from the prompt section and
/// passed to `SlashDotNode` trait methods.
pub struct PathArgs {
    /// Resolved node id path so far (e.g. `["tree", "compact"]`).
    pub ids: Vec<String>,
    /// The prompt section for this level.
    pub suffix: String,
    /// Parsed arguments from this level's section.
    pub args: Args,
}

/// Parsed arguments from a level's section.
pub struct Args {
    /// Dot modifiers: `(key, raw_value)` pairs. Single-word values only.
    pub modifiers: Vec<(String, String)>,
    /// Positional tokens (non-modifier, non-selection words). These are
    /// fuzzy search terms at listing level and positional parameters at
    /// leaf level.
    pub positional: Vec<String>,
}

impl Default for Args {
    fn default() -> Self {
        Args {
            modifiers: Vec::new(),
            positional: Vec::new(),
        }
    }
}

/// The rows a node contributes to its parent's listing.
pub struct DataResponse {
    pub rows: Vec<Row>,
    /// Ids of rows that are "currently active" (`*` marker, §5.2).
    pub active_ids: Vec<String>,
}

/// Error returned by `data()` when the node cannot produce rows.
#[derive(Debug)]
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
#[derive(Debug)]
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
    /// No key action — used when Tab drills but does not forward a key.
    None,
    /// Forwarded Enter from a parent listing. Leaf nodes submit on this;
    /// listing nodes ignore it (stops recursion).
    MaybeSubmit,
}

// --- Public trait for host implementations ---

/// Host implementations of menu nodes. Each command is one `impl SlashDotNode`.
/// The engine handles all traversal, cursor, and child management internally.
///
/// All methods have default implementations (no-op / empty). Override only
/// what the node needs.
pub trait SlashDotNode {
    /// The rows this node contributes to its parent's listing.
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
    fn prepare(&mut self, epoch: u64, path_args: &PathArgs) -> SubmitResult {
        self.submit(epoch, path_args)
    }

    /// Non-selectable header lines (replaces v2 DetailProvider).
    /// Default: empty. The engine truncates based on terminal size.
    fn header(&mut self, _path_args: &PathArgs) -> Vec<String> {
        Vec::new()
    }

    /// Field metadata for this level's children's rows (§2.1 / §8.1).
    /// Declared once on the parent; children conform. The engine uses
    /// these specs for filter/sort. Default: no extra fields beyond the
    /// implicit `id` and `description`.
    fn fields(&self) -> Vec<crate::fields::FieldSpec> {
        Vec::new()
    }

    /// Presentation columns for table rendering (D6 / §6). When
    /// non-empty, the engine renders rows as a table with these column
    /// definitions (header labels, alignment, sizing). When empty,
    /// rows render as plain `id — description` text. Default: empty.
    fn columns(&self) -> Vec<crate::provider::ColumnDef> {
        Vec::new()
    }
}

// ---------------------------------------------------------------------------
// StaticListingNode — convenience impl for listing nodes with static rows.
// ---------------------------------------------------------------------------

/// A row definition for [`StaticListingNode`].
#[derive(Debug, Clone)]
pub struct StaticRow {
    pub id: String,
    pub desc: String,
}

impl StaticRow {
    pub fn new(id: impl Into<String>, desc: impl Into<String>) -> Self {
        StaticRow {
            id: id.into(),
            desc: desc.into(),
        }
    }
}

/// A convenience `SlashDotNode` for listing-level commands that return a
/// fixed set of rows (no dynamic data, no submit).
///
/// Use this instead of writing a dedicated `impl SlashDotNode` for simple
/// listing nodes like `/thinking`, `/persona`, or any static menu entry.
///
/// For active markers, pass a shared `HashSet<String>` via
/// [`StaticListingNode::with_active`].
///
/// # Example
/// ```ignore
/// let node: SlashDotMenuNode = StaticListingNode::new(&[
///     StaticRow::new("thinking", "set thinking level"),
///     StaticRow::new("model", "switch model"),
/// ]).into();
/// node.add_child(child.into());
/// ```
pub struct StaticListingNode {
    rows: Vec<StaticRow>,
    header_lines: Vec<String>,
    active_ids: std::rc::Rc<std::cell::RefCell<std::collections::HashSet<String>>>,
    field_specs: Vec<crate::fields::FieldSpec>,
    column_defs: Vec<crate::provider::ColumnDef>,
}

impl StaticListingNode {
    /// Create from a static slice of rows.
    pub fn new(rows: &[StaticRow]) -> Self {
        StaticListingNode {
            rows: rows.iter().cloned().collect(),
            header_lines: Vec::new(),
            active_ids: std::rc::Rc::new(std::cell::RefCell::new(std::collections::HashSet::new())),
            field_specs: Vec::new(),
            column_defs: Vec::new(),
        }
    }

    /// Set the header lines shown above the listing.
    pub fn with_header(mut self, header: Vec<String>) -> Self {
        self.header_lines = header;
        self
    }

    /// Set the active IDs (e.g. current selection). The `Rc<RefCell<...>>`
    /// can be shared with child nodes or external state.
    pub fn with_active(
        mut self,
        active: std::rc::Rc<std::cell::RefCell<std::collections::HashSet<String>>>,
    ) -> Self {
        self.active_ids = active;
        self
    }

    /// Set field metadata for filter/sort on this level's children.
    pub fn with_fields(mut self, specs: Vec<crate::fields::FieldSpec>) -> Self {
        self.field_specs = specs;
        self
    }

    /// Set column definitions for table rendering. When non-empty, the
    /// table header row shows these column labels and the table renders
    /// only these columns. Use `"id"` for the row id, `"description"`
    /// for the row description, or any custom field name.
    pub fn with_columns(mut self, cols: Vec<crate::provider::ColumnDef>) -> Self {
        self.column_defs = cols;
        self
    }
}

impl SlashDotNode for StaticListingNode {
    fn data(&mut self, _epoch: u64, _path_args: &PathArgs) -> DataResult {
        let rows: Vec<Row> = self
            .rows
            .iter()
            .map(|r| Row::new(&r.id).desc(&r.desc))
            .collect();
        let active: Vec<String> = self.active_ids.borrow().iter().cloned().collect();
        Ok(std::rc::Rc::new(DataResponse {
            rows,
            active_ids: active,
        }))
    }

    fn header(&mut self, _path_args: &PathArgs) -> Vec<String> {
        self.header_lines.clone()
    }

    fn fields(&self) -> Vec<crate::fields::FieldSpec> {
        self.field_specs.clone()
    }

    fn columns(&self) -> Vec<crate::provider::ColumnDef> {
        self.column_defs.clone()
    }
}
