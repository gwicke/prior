//! UI chrome the REPL composes into the frame.
//!
//! [`bar`] holds the status-bar element and the fit that turns the shaped
//! status data into a row. Overlay/menu elements come from
//! `slashdot_menu::elements`: an element is built from raw data and formats
//! inside `render(h, w)` at the width being composed for, so it holds no
//! painted row; the repl appends `RenderPlan::elements`.

pub(crate) mod bar;
