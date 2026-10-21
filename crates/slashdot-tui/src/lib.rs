//! The drawing stack between content and the terminal (design
//! `doc/261003_output_drawing_refactor.md`): [`render`] composes
//! [`render::Element`]s into a [`render::Frame`] through the
//! [`render::RenderManager`] hub, [`draw`] owns the screen and [`termio`] owns
//! the tty — one reader thread, the size watch, DSR. [`ansi`] holds the byte /
//! width / escape transforms and the write wrappers; [`ansi::display_width`]
//! is the shared string-measure the layers and `slashdot_menu` agree on.
//! [`wrap`] is the width-fitting split/reflow over `&str`, built on that
//! same measure.
//!
//! The crate knows nothing of the layers above: no repl, no content, no
//! menu chrome. Elements arrive through the `RenderManager`'s configure/render
//! verbs; everything below that interface is this crate's own.

pub mod ansi;
pub mod draw;
pub mod render;
pub mod termio;
pub mod wrap;
