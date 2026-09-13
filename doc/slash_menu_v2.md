# Slash menu v2 design doc
Goals
- Regular and data driven: Replace custom key bindings with data structure
  defining the structure, leaving low-level key handling to the generic menu
  tooling
- Syntax driven, robust: Prompt fully represents menu state

V1 design & principles: slash_menu.md - v2 refines and generalizes these UX
principles

##  represent filter and selection list cursor state in path
Currently: Hidden, need special logic to return to previous list / selection
(e.g. in tree view).

Idea: 
- `/tree [search terms] id` - make `<id>` reflect the selection state
- `/tree [search terms] id ` - show sub-menu (trailing space)
- `/tree [search terms] id switch` - switch action menu selected but not shown
- `/tree [search terms] id switch ` - switch action menu shown (trailing
  space)
- `/tree [search terms] id compact llm .prompt:some long prompt` - compaction menu
  shown, llm entry selected; enter calls llm leaf handler
- `/tree [search terms] id switch direct`
- `/tree [search terms] id switch manual`
- `/tree [search terms] id switch llm .prompt:args`
Trickier:
- `/set [search terms] <key> /value` - plain value or / menu for model
  selection

## mixing of filtering and formal identifiers
- last in hierarchy level
- no spaces in ids (usually uids)

## extended filter syntax
Currently
- `path:.` -> `.path:.`

Future: model selection
- `.price:free` - sorted by popularity, or mix of pop + relevance to query
  strings
- `.price:in<0.10;out<0.6` - sorted by popularity
- `.sort:new` - new models
- `.sort:new,pop`


# Menu kinds
## Common
- tree structure: each entry can have children - implies syntactical nesting
- key, description for each entry - fuzzy search across both
- Optional modifiers
    - string key, string or number values (typed)
    - nominate some as default on tab completion (including value): llm
      compaction .prompt, session .path filter etc
    - have their own auto complete on keys (`.*`), string values on enum /
      default values
    - parsing consumes following words into value up to next dot modifier,
      valid id (positional), or end of prompt
- handlers registered on leafs

## Static / data driven only
- Top level `/`, `/set` etc - purely driven by data
- Only need handlers on leafs
  - args: positional ids from path, modifier key-values

## Dynamic data
- `/tree`, `/model`, `/set default_model`
- data fetch handler: 
    - designates dynamic id for each row (string, e.g. uuid)
        - cursoring through a listing updates id in prompt (last position)
        - when the last word in prompt is a valid ID in current listing view &
          a trailing space is present / added, then move to next level of
          hierarchy / display sub-tree menu
    - optional description string field for each entry
    - optional additional key-value fields (string key, values [string|number])
        - configure subset for display in table (list of keys)
        - pervasive filtering / sorting via `.` modifiers, corresponding to
          all fields
            - auto-complete / filter `string` values based on actual dataset
              values; optional manual override to pre-defined enum set
- fuzzy search defaults across ids, description fields
- optional leaf handler for final accept (enter with valid ID present, e.g.
  /model dialog); when the leaf handler is absent, enter behaves like tab
  (adds trailing space, moves to child menu)

