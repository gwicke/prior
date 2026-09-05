- [x] bash tool error return: report error code
- [x] restore model on `/session <id> resume` (now via pre-fill, not silent)
- [x] Compaction unification (Phase 7)
  - [x] /compact command — same as `/tree <id> compact` but at current position.
        Resolves `target_id` to the last user message on the active branch and
        commits a canonical `/tree <last_user_id> compact <mode>` line so the
        existing /tree handler is the single entry point. Shared
        `run_compaction` backend; three modes (truncate / text-only / llm);
        same `llm_compact_prompt` setting; same prompt-customization path.
  - [ ] `/tree <id> switch --summarize` - use same menu / mechanism as others
        (backend ready: `CompactionTarget::Abandoned`; UI TBD)
        `/tree <id> switch`, `/tree <id> switch llm <prompt>`, `/tree <id> switch simple`
- [ ] Thinking-level restore on `/session <id> resume` (single input field
      currently can only carry one prefill — model or thinking. Bundle both
      into a slug syntax like `/thinking <level> & /model <slug>` or a new
      `/resume-config <slug>` once the need arises.)
