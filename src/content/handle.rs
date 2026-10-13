//! The `Content` handle (step 3c of the output-drawing refactor): the agent-
//! facing owner of the transcript's content. It holds the content boundary
//! state, the width watch, the streaming renderers, and the byte sink (`Draw`),
//! and — as the content methods move onto it — formats content at the watched
//! width and writes through the sink.
//!
//! Frame methods stay on `Output` for now; `ConsoleOutput` carries a temporary
//! pass-through to `self.content.draw` for them until step 4 makes
//! `RenderManager` the frame front door.

use std::sync::Mutex;

use serde_json::Value;
use tokio::sync::watch;

use crate::draw::Draw;
use crate::settings::ShowReasoningHandle;
use crate::termio::{Caps, TermSize};
use markdown_stream::StreamParser;
use markdown_terminal::{Renderer, Theme};

use super::composition::ContentBoundary;

/// The agent-facing content owner (step 3c).
///
/// The fields are `pub(crate)` while the content methods still live on
/// `ConsoleOutput` and reach them through `self.content.…`; they tighten to
/// private once those methods move onto `Content` (3c-b).
pub struct Content {
    /// The live screen: byte sink plus the bookkeeping of what is believed to
    /// be on it. Writer before cache, always — see [`Draw`].
    pub(crate) draw: Draw,
    /// This instance's view of the size watch (step 2) — the same channel
    /// `draw` holds, so the content path's width reads and the frame path's
    /// geometry cannot disagree. In production a subscriber of the process-wide
    /// channel; in tests, a private seeded one.
    pub(crate) size: watch::Receiver<TermSize>,
    /// Per-stream capabilities, probed once (step 2): colour and the reasoning
    /// routing read this instead of calling `is_terminal()` per site.
    pub(crate) caps: Caps,
    /// Where reasoning goes: `true` = stderr, so a consumer of
    /// `picocode "q" | cat` can drop it (`2>/dev/null`) or merge it (`2>&1`)
    /// without touching stdout. Decided once, here.
    pub(crate) reasoning_on_stderr: bool,
    /// Incremental GFM markdown parser for the current response (TTY only).
    pub(crate) parser: Mutex<StreamParser>,
    /// Incremental terminal renderer (TTY only) for the answer.
    pub(crate) renderer: Mutex<Renderer>,
    /// Incremental parser/renderer for *thinking* content, kept separate from
    /// the answer pair so the two have independent state.
    pub(crate) thinking_parser: Mutex<StreamParser>,
    pub(crate) thinking_renderer: Mutex<Renderer>,
    /// Live `show_reasoning` display mode, read on every thinking event.
    pub(crate) show_reasoning: ShowReasoningHandle,
    /// The most recent tool call's `(name, args)`, captured in
    /// `display_tool_call` and read back in `display_tool_result` so the
    /// result can be rendered tool-aware.
    pub(crate) last_tool: Mutex<Option<(String, Value)>>,
    /// The content boundary/separator state (step 3a): the last declared kind,
    /// its segment-closed flag, and the held blank rows.
    pub(crate) boundary: ContentBoundary,
}

impl Content {
    /// Build the content half from its per-process inputs: the `Draw` sink and
    /// size watch are created together (they share the watch), the caps are
    /// probed once, and the renderers start blank at a placeholder width.
    pub(crate) fn new(
        size: watch::Receiver<TermSize>,
        show_reasoning: ShowReasoningHandle,
    ) -> Self {
        let caps = Caps::probe();
        Self {
            draw: Draw::new(size.clone()),
            size,
            caps,
            reasoning_on_stderr: !caps.primary,
            parser: Mutex::new(StreamParser::new_gfm_stream()),
            renderer: Mutex::new(Renderer::new(Theme::no_color(), 80)),
            thinking_parser: Mutex::new(StreamParser::new_gfm_stream()),
            thinking_renderer: Mutex::new(Renderer::new(Theme::no_color(), 80)),
            show_reasoning,
            last_tool: Mutex::new(None),
            boundary: ContentBoundary::new(),
        }
    }

    /// This instance's width: its subscribed watch, with the same 20-column
    /// floor as the [`crate::output::term_width`] free function.
    pub(crate) fn width(&self) -> usize {
        self.size.borrow().width.max(20)
    }

    /// This instance's height — symmetric to [`Self::width`].
    pub(crate) fn height(&self) -> usize {
        self.size.borrow().height.max(1)
    }
}
