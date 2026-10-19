//! Narrow prompt-buffer interface.
//!
//! The host implements this over its input editor; the crate never touches
//! editor internals (no cursor ops, no spans, no editing primitives — the
//! interpreter always rewrites the whole prompt via `set_text`).

pub trait PromptBuffer {
    /// The current prompt text.
    fn text(&self) -> String;
    /// Replace the whole prompt text.
    fn set_text(&mut self, s: &str);
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Headless stand-in used across the crate's tests.
    #[derive(Default)]
    pub(crate) struct MockBuffer {
        pub s: String,
    }

    impl PromptBuffer for MockBuffer {
        fn text(&self) -> String {
            self.s.clone()
        }
        fn set_text(&mut self, s: &str) {
            self.s = s.to_string();
        }
    }

    #[test]
    fn round_trips() {
        let mut b = MockBuffer::default();
        b.set_text("/tree ");
        assert_eq!(b.text(), "/tree ");
    }
}
