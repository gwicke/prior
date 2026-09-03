- [x] bash tool error return: report error code
- [x] restore model on `/session <id> resume` (now via pre-fill, not silent)
- [ ] Compaction unification
  - [x] /compact command - same as `/tree <id> compact` but at current position
  - [ ] `/tree <id> switch --summarize` - use same menu / mechanism as others
- [ ] Thinking-level restore on `/session <id> resume` (single input field
      currently can only carry one prefill — model or thinking. Bundle both
      into a slug syntax like `/thinking <level> & /model <slug>` or a new
      `/resume-config <slug>` once the need arises.)