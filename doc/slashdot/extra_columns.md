# Extra columns — the sorted-on field joins the listing table

Status: **DONE** (sort producer; a `.columns:` modifier and filter columns are
follow-ups on the same mechanism)

## Problem

A listing node declares its table columns once (`SlashDotNode::columns()`), so
the displayed column set is static. The pressing case is sorting: `.sort:price`
reorders the rows, but the field the user sorted on may not be on screen.

## Mechanism

The view handle (`ViewRows`) carries the columns to show **in addition** to the
node's declared ones:

```rust
extra_columns: Option<Vec<ColumnDef>>
```

`ViewRows` is minted by the rows pipeline together with the compiled query, so
the extras cannot go stale against the rows they describe — the sort field and
the row order are minted in one struct. The memoized pipeline hands back the
cached view on a memo hit, extras included (the memo key is the suffix, and
the suffix determines the query).

They are *declarations*, not a contract: the producer does not check whether
the node already shows the field. `NodeEntry::resolve_columns`, at plan time,
settles it:

- **nothing to add** (no extras, or every extra is already displayed) ⇒ the
  declared columns are handed back **unchanged** — an empty list stays empty,
  so the renderer keeps its implicit id/description fallback. Header
  behaviour is exactly as before the mechanism existed.
- an extra whose field is already displayed ⇒ dropped; the node's own column
  wins (its header, alignment, sizing).
- two extras naming one field ⇒ the first declaration wins.
- otherwise ⇒ appended, in order; the node's own columns keep their order and
  layout. An added column takes the **base's wrap style** (a single
  non-wrapping column would flip the whole table to single-line rendering).
- a level that declares **no** columns ⇒ the implicit id/description pair
  becomes the explicit base (`ColumnDef::new("id", "id")` etc. — the same
  labels the implicit path uses for its width floor, so the layout is
  identical), and the extra is appended to it.

The added column carries a label (the field's own key), so a level that showed
no header before shows one once a column is added. That is deliberate: the
added column needs a name.

## Producer

`NodeEntry::ensure_rows` (the rows pipeline) declares the sorted-on field at
the mint, where `query.sort` and the field space are in hand:

```rust
ColumnDef::new(field, field)              // header = the field's key
    .align(if is_number { Right } else { Left })
```

The declaration is unconditional; the resolution drops it when the node
already shows the field. The resolution and the consumer live in `NodeEntry`
too — one line in `view_plan`, next to the producer:

```rust
let columns = Self::resolve_columns(self.inner.columns(), rows.extra_columns());
```

which feeds `ViewRequest.columns` → `RenderPlan.columns` → the elements,
unchanged downstream. The view side is purely the carrier: the
`extra_columns` field, its builder and accessor, plus `implicit_columns()`
(which keeps its labels next to the renderer's own fallback).

## Follow-ups on the same mechanism

- **`.columns:a,b`**: a modifier naming fields to add; same declaration, same
  resolution.
- **Filter columns**: filtering on a field that is not displayed is the same
  shape — one more producer.

## Known limitation

Fuzzy match indices are computed over the node's declared fields only, so an
added column's text is not highlighted. Widening the displayed-field list
would change fuzzy scores for any query combining a sort with a fuzzy term;
left as a follow-up decision.
