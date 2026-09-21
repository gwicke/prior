//! v2.5 `SlashDotNode` implementations for picocode commands.
//!
//! Each command is one `impl SlashDotNode`. The engine handles traversal,
//! cursor, and child management internally.

use std::rc::Rc;

use crate::slashdot_menu::node_impl::{
    DataResponse, DataResult, PathArgs, SlashDotNode, SubmitResult,
};

// ---------------------------------------------------------------------------
// ThinkingEnumNode — returns all thinking levels from a single node.
//
// The parent listing shows off/low/med/high as rows. On submit, the
// selected level is read from path_args (the drilled row ID) and written
// to the shared cell.
// ---------------------------------------------------------------------------

pub struct ThinkingEnumNode {
    /// Read the current thinking level.
    pub active_fn: Box<dyn Fn() -> String>,
    /// Write the selected thinking level. `Ok(())` on success.
    pub submit_fn: Box<dyn FnMut(&str) -> Result<(), String>>,
}

impl SlashDotNode for ThinkingEnumNode {
    fn data(&mut self, _epoch: u64, _path_args: &PathArgs) -> DataResult {
        let active = (self.active_fn)();
        let levels: &[(&str, &str)] = &[
            ("off", "no extra reasoning"),
            ("low", "minimal reasoning"),
            ("med", "balanced reasoning"),
            ("high", "maximum reasoning"),
        ];
        let rows = levels.iter()
            .map(|(l, d)| crate::slashdot_menu::fields::Row::new(*l).desc(*d))
            .collect();
        Ok(Rc::new(DataResponse {
            rows,
            active_ids: vec![active],
        }))
    }

    fn submit(&mut self, _epoch: u64, path_args: &PathArgs) -> SubmitResult {
        let level = path_args.ids.last().cloned().unwrap_or_default();
        match (self.submit_fn)(&level) {
            Ok(()) => SubmitResult::Done(None),
            Err(msg) => SubmitResult::Done(Some(msg)),
        }
    }
}
