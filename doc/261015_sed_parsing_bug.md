# SED tool syntax issues

## Document `sed -f` syntax - no escapes

Add this in the tool documentation

## The one genuine bug: raw `/` inside a bracket expression

The parser scanning for the pattern's closing delimiter doesn't skip `[...]`, so a literal `/`
inside a char class terminates the regex early:

```text
s|a[/]b|X|g          → parse error: unknown command: 'X'
s/[^/ ]*/X/g         → parse error: unknown command: 'X'
s/, impl doc [§0-9][0-9A-Za-z./§-]*//g
                    → parse error: unterminated regex (expected closing '/')
```

GNU sed accepts all three (`sed -e 's|a[/]b|X|g'` → `aaXbb ccc`). It also bites with the `/`
delimiter even though the class "contains" the delimiter legitimately — that's the error I hit in my
first batch attempts (`[0-9A-Za-z./§-]`), which is why I spent so long fighting "missing
command"/"unknown command: '('" errors that had nothing to do with parens. Note it hits the
**replacement** side too, presumably for the same reason — though I didn't construct a failing
replacement-side case here.

  Workarounds that work through the tool, confirmed:

- `\/` inside the class: `s/[a\/b]*a/X/g` works (GNU sed accepts the escape inside `[]` too).
- Split the rule: drop that expression from the batch and match the content differently (e.g. `.*`
  instead of a class containing `/`).
