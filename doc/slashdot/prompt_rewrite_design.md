# Slash menu v2.5 prompt rewrite extension
We have a need to rewrite the prompt in a few situations:

- `/compact`+enter|tab -> `/tree <id> compact`, with `<id>` the latest / active session
  message. Leaf node.
- `/session` + tab -> `/session .path:.` - default filter on current
  directory. Listing node.
- Edit leaf node, e.g. `/set tool_call_limit`+tab/enter: Pre-fill current value in prompt
- `/tree <id> compact prompt` + tab / enter -> pre-fill default prompt

## Design
- New SlashDotNode trait method `drill_rewrite(mut &self, PathArgs) -> Option<String>`
- Default: return None
