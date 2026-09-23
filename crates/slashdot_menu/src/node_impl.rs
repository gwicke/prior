//! Node implementation trait (v2.5 design §2).
//!
//! Host applications implement `SlashDotNode` for each menu command.
//! The engine handles all traversal, cursor, and child management internally.

use crate::fields::Row;
use crate::render::RenderPlan;
use std::ops::Range;

// --- Public types used by SlashDotNode implementors ---

/// Rendering context threaded down `node_traverse` by reference. Carries
/// display metrics the node needs for layout; not related to paths or
/// path args (an app-level context may be added alongside later).
#[derive(Debug, Clone, Copy)]
pub struct RenderContext {
    /// Terminal width (columns) for layout/formatting decisions.
    pub term_width: usize,
}

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

/// Parsed arguments from a level's section, as byte ranges indexing into
/// [`PathArgs::suffix`] (no string copies).
#[derive(Debug, Default)]
pub struct Args {
    /// Dot modifiers as `(key, value)` byte-range pairs into `suffix`.
    /// A bare `.key` yields an empty value range.
    pub modifiers: Vec<(Range<usize>, Range<usize>)>,
    /// Positional tokens (non-modifier, non-selection words) as byte
    /// ranges into `suffix`. Fuzzy search terms at listing level and
    /// positional parameters at leaf level.
    pub positional: Vec<Range<usize>>,
}

impl PathArgs {
    /// Convenience constructor (mainly for tests / manual construction).
    pub fn new(ids: Vec<String>, suffix: impl Into<String>) -> Self {
        PathArgs {
            ids,
            suffix: suffix.into(),
            args: Args::default(),
        }
    }

    /// Positional args as string slices into `suffix`.
    pub fn positional(&self) -> impl Iterator<Item = &str> + '_ {
        self.args
            .positional
            .iter()
            .map(move |r| &self.suffix[r.clone()])
    }

    /// Dot-modifier `(key, value)` pairs as string slices into `suffix`.
    /// The value is empty for a bare `.key`.
    pub fn modifiers(&self) -> impl Iterator<Item = (&str, &str)> + '_ {
        self.args.modifiers.iter().map(move |(k, v)| {
            (
                &self.suffix[k.clone()],
                &self.suffix[v.clone()],
            )
        })
    }

    /// The full prompt for the host: `ids` joined, then `suffix`.
    ///
    /// Mirrors the old free `full_prompt()` — an empty suffix yields the
    /// joined ids alone; a suffix starting with a space is concatenated
    /// directly (the space comes from the drill), otherwise a separator
    /// space is inserted.
    pub fn full_prompt(&self) -> String {
        self.prompt_with(&self.suffix)
    }

    /// Like [`full_prompt`](Self::full_prompt) but with a replacement
    /// suffix (Esc rewrites, leaf value rewrites, etc.).
    pub fn prompt_with(&self, suffix: &str) -> String {
        if self.ids.is_empty() {
            return suffix.to_string();
        }
        let base = self.ids.join(" ");
        if suffix.is_empty() {
            base
        } else if suffix.starts_with(' ') {
            format!("{}{}", base, suffix)
        } else {
            format!("{} {}", base, suffix)
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
    /// Menu done; close the overlay. (message, self-documenting prompt).
    DoneSuccess(Option<String>, String),
    /// Error; close the overlay. (message, self-documenting prompt).
    DoneError(String, String),
}

// --- Public trait for host implementations ---

/// Host implementations of menu nodes. Each command is one `impl SlashDotNode`.
/// The engine handles all traversal, cursor, and child management internally.
///
/// All methods have default implementations (no-op / empty). Override only
/// what the node needs.
pub trait SlashDotNode {
    /// The rows this node contributes to its parent's listing.  *All* leaf
    /// nodes need to implement this, and should return one or more rows with an `id` and
    /// `description`. 
    /// Handled automatically for `StaticListingNode`, so higher levels of the hierarchy which have
    /// leaf nodes as children.
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

    /// Optional confirm / two-phase-commit hook. Called on the **first**
    /// Enter; if it returns `Prepared`, the engine sets `confirm_pending`
    /// and the **second** Enter calls `submit()` instead.
    ///
    /// **Default:** delegates straight to `submit()` (single-phase).
    ///
    /// Override **only** for genuine two-phase workflows (commit/prepare →
    /// edit/accept, or confirm dialogs).  Do **not** override just to
    /// show a header — use `header()` for that.
    fn prepare(&mut self, epoch: u64, path_args: &PathArgs) -> SubmitResult {
        self.submit(epoch, path_args)
    }

    /// Non-selectable header lines (replaces v2 DetailProvider).
    /// Default: empty. The engine truncates based on terminal size.
    /// `render_ctx` carries display metrics (terminal width) for layout.
    fn header(
        &mut self,
        _path_args: &PathArgs,
        _render_ctx: &RenderContext,
    ) -> Vec<String> {
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

    /// Optional prompt rewrite on drill. Called when a parent listing
    /// drills into this node (user presses Tab or Enter on a parent
    /// selection), before data refresh and parse.
    ///
    /// Returns `Some(new_full_prompt)` to replace the current prompt
    /// and restart traversal from root, or `None` for default behavior
    /// (listing: show listing; leaf: submit on Enter drill).
    /// Use [`PathArgs::full_prompt`] / [`PathArgs::prompt_with`] to
    /// build the rewritten prompt from `path_args.ids`.
    ///
    /// `&mut self` provides full access to shared state (e.g.
    /// `MenuShared` for session tree queries, settings for pre-fills).
    ///
    /// **Default:** `None` — no rewrite.
    fn drill_rewrite(&mut self, _path_args: &PathArgs) -> Option<String> {
        None
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

/// A convenience `SlashDotNode` for purely static listing-level commands that return a fixed set of
/// rows (no dynamic rows, no active ids, no submit, no dynamic behavior).
///
/// Use this instead of writing a dedicated `impl SlashDotNode` for pure static listing nodes like
/// `/thinking`, `/persona`. However, if you feel like extending this, you probably want to
/// implement your own simple SlashDotNode instead. Every SlashDotNode impl that has children gets
/// full listing functionality from the engine for free; there is nothing special about
/// StaticListingNode when it comes to listings.
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

    fn header(
        &mut self,
        _path_args: &PathArgs,
        _render_ctx: &RenderContext,
    ) -> Vec<String> {
        self.header_lines.clone()
    }

    fn fields(&self) -> Vec<crate::fields::FieldSpec> {
        self.field_specs.clone()
    }

    fn columns(&self) -> Vec<crate::provider::ColumnDef> {
        self.column_defs.clone()
    }
}
