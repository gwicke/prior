//! The content layer: classification, the separator matrix, and content
//! rendering. None of it crosses
//! below — the only vocabulary going down is `Stream` (the write's kind), so
//! `render` / `draw` stay generic
//! (`doc/261003_output_drawing_refactor.md`).

pub(crate) mod composition;
pub(crate) mod handle;
pub(crate) mod markdown;
pub(crate) mod panel;
pub(crate) mod preview;
pub(crate) mod text;

pub(crate) use composition::{boundary_spacer, separator_line, ContentDisplayKind};
// `pub`: `AgentConfig.content` is a public field, so the handle has
// to be nameable from the binary too. Its state stays crate-private.
pub use handle::Content;
