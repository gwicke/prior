# Phase 1 features

## slash commands and pervasive autocomplete / fuzzy search

- Add slash commands for at least /model selection, /thinking level, and /help.
- When typing `/`, display a list of all slash commands. Further characters
  filter with fuzzy matching (fzf like behavior, propose crates). The same
  filtering is applied to all other auto-completes.
- The auto-complete pop-up can use an alt buffer, and it is okay to stop
  scrolling.
- For the /model selection list, fetch all available models from all providers
  (with short term caching, say 5 minutes) including details like token limits
  and cost. Normalize all model names as {provider}:{modelname}, and present a
  single list across all providers that includes token limits and cost.
  - Later, support prioritizing specific models so they show up at the top of
    the selection list by default, before any fuzzy filter is entered.

## Leverage native terminal scrolling

Show the full session history in a container; do not re-implement scrolling
within that container. Break lines at word boundaries. Re-draw the container
when terminal size changes, or when the agent process is resumed. Try to avoid
forcing scroll to the end on new output when the user reads at an earlier
point in the session history.

## Support ctrl-z and ctrl-c like cooked mode

- Install handlers, send the signals. Do not use unsafe code for this. Use
  existing dependencies when possible.

## Multi-line text input

- Support multi-line text input in the prompt. Only an actual <enter> key
  press should submit the text - not pasted newlines. `<shift-enter>` should add
  a newline. Regular arrow key navigation (also across lines) and edit commands.
  Text wrap at word boundaries. Expand the input box to fit all text, pushing
  session content upward.
