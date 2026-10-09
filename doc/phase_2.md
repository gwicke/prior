# Next stage todos

## session output

- thinking_audit.md - surface thinking content
- better tool formatting:
  - show full bash command, shorten preamble to just $

## Sandboxing / commands

- Require sandbox
- Run `grep` in sandbox
- Add `!` and `!!` commands, run in sandbox

## Agent changes

- Enable caching
- Track token usage / cache hit rate - new /usage command
- Compaction similar to ultracompact
  - drop reasoning content

## / menu

New entries

- /settings
  - default model (with provider string)
  - security mode: yolo / confimations for destructive tool execution
  - tool-call-limit <TOOL_CALL_LIMIT> Maximum number of tool calls per prompt
    [default: 50]
  - default persona
  - default thinking level
  - show thinking content
- /persona <PERSONA>
  Choose a persona for the agent. Available built-in personas:
  - architect A hands-on software architect who loves Van Halen and hard rock.
  - strict A very strict software engineer who operates with Swiss clock
    precision.
  - security An all-knowing security analyst who loves Bruce Schneier facts.
  - zen A Zen Master who views coding as a form of meditation.
  - hacker A chaotic good hacker obsessed with elegant hacks and performance.
  - guru A Silicon Valley guru obsessed with disruption and scale.
  - sysadmin A grumpy, old-school sysadmin who has seen it all.
  - academic A formal academic who cites papers and prefers theoretical
    correctness.
  - hustler A startup hustler who moves fast and breaks things.
  - craftsman    A web craftsman obsessed with accessibility and the open web.
  - sre An SRE ninja who focuses on reliability and observability.
  - maintainer A patient open source maintainer who loves documentation.
  - tester A destructive QA tester who lives to find edge cases.
- /usage
  - token and $ usage, cache hit ratio
- /info
  - version
  - configured providers
- sandbox
  - add_ro
  - add_rw
  - remove

cancellation for tool commands
preservation of context across models / on error
    add optional debug logging of context sent / raw messages
    when a turn errors, send full previous model output of that turn for
    continuity
    verify that cancelled turns still preserve as much context as possible
caching improvements esp. for deepseek, openrouter
session storage
  resume / tree
  /name
