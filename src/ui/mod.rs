//! UI chrome the REPL composes into the frame.
//!
//! This module used to mint the overlay elements a menu or confirmation is
//! built from (an `overlay_elements*` entry plus the `SectionItem` /
//! `RowsItem` elements, painted at compose width into a `PaintedSection`).
//! That was the compose-width snapshot the design retires: an element is
//! built from raw data and formats inside `render(h, w)` at the width being
//! composed for, so it has no painted row to hold beforehand. The mint and
//! its tests moved to `slashdot_menu::elements` in step 4h-d, where the
//! view it reads lives — the repl now appends `RenderPlan::elements(1)`.
//!
//! [`bar`] is what remains here: the status-bar element and the fit that
//! turns the shaped data into a row (step 4h-c).

pub(crate) mod bar;
