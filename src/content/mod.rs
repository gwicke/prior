//! The content layer: classification, the separator matrix, and content
//! rendering (steps 3a–3b of the output-drawing refactor). None of it crosses
//! below — the only vocabulary going down is `Stream` (main / meta), so
//! `render` / `draw` stay generic
//! (`doc/261003_output_drawing_refactor.md`).

pub(crate) mod composition;
pub(crate) mod handle;
pub(crate) mod markdown;
pub(crate) mod panel;
pub(crate) mod preview;
pub(crate) mod text;

pub(crate) use composition::{boundary_spacer, separator_line, ContentDisplayKind};
pub(crate) use handle::Content;
