# /tree v2.5 design

## Structure
- root menu
  - StaticListingNode row = "tree" / description from v1
    - columns: description (no wrap)
    - children:
      - SessionListNode
        - data() returns list of session messages: id, description, timestamp;
          similar in structure to SessionListNode
        - header(): show full message text
        - children:
          - StaticListingNode row="switch"
            - children
              - SimpleTreeSwitchNode
                - submit: AsyncOp if switch is async (TODO: check)
              - SummarizedTreeSwitchNode: DEFERRED for later - will try to
                reuse compaction logic (see below)
          - StaticListingNode row="compact"
            - children:
              - ManualCompactionNode 
                - data() row = manual | manual compaction message with editor
                - prepare() prep + editor, submit() 2PC
              - JustTextCompactionNode (renamed from "text only")
                - data() row = justtext | all user and model text, no tool calls and reasoning; opens editor
                - prepare() + editor, submit() 2PC
              - PromptCompactionNode (renamed from "llm")
                - data() row = prompt | summarize with editable llm prompt
                - prepare() + editor, submit() 2PC

## `/tree ` listing format
We are rendering a tree structure, so the goal is to communicate the structure
visually, while using minimal space. The current v1 design uses indentation to
convey structure, and subtle alternating background color changes to separate
branches at the same level.

Additionally, system messages like compactions are currently rendered in a
different text color.

With the v2.5 generic list rendering, the goal is to convey the same
information without the custom colors. Proposal:

- Still use indentation, but use box drawing characters to draw a tree
  structure for sub-trees (fork points, currently four spaces of indent per
  level). Use only indentation for user message / system response distinction.
- System events like compaction: Prefix with [COMPACT], [MODEL] for better
  visibility. Same indentation as user messages (these are user initiated).


## Implementation plan

## Deferred / follow ups
- Switch with summarization / compaction
