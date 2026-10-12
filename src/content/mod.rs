//! The content layer: classification and the boundary/separator bookkeeping.
//!
//! Step 3a of the output-drawing refactor
//! (`doc/261003_output_drawing_refactor_impl.md`): the transcript's logical
//! content kinds, the separator matrix, and the boundary state move out of
//! `output.rs` / `draw.rs` into `content/`. None of it crosses below — the only
//! vocabulary that goes down is `Stream` (main / meta), so `render` / `draw`
//! stay generic (`doc/261003_output_drawing_refactor.md`).

pub(crate) mod composition;

pub(crate) use composition::{
    boundary_spacer, separator_line, ContentBoundary, ContentDisplayKind,
};
