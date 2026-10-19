# Prior: Small coding agent

Injecting priors into your code since 2026.

## Features

- Pi compatible session storage (`~/.config/prior/sessions`)
- Block based single & multi file batch editing with thoughtful diagnostics for reliable and
  efficient error recovery.
- `bsed` batch editing tool - produces block syntax, can be fed to the edit tool. Also a stand-alone
  binary.
- Powerful slash menu system with pervasive fuzzy filtering, advanced sorting and filtering
- Tree navigation and compaction similar to `pi agent`. Manual / text-only / prompt based
  compactions / summarization.
- Focus on token efficiency in system prompt, tools.

## Quick start

```bash
cargo install prior-agent
export DEEPSEEK_API_KEY="..."
prior --help
```

## History

Originally based on [picocode](https://github.com/jondot/picocode) by @jondot.
