# Sandbox design & implementation

### 9.1 Prerequisite: thread prompt suffix through `refresh_data()`

**Problem:** `refresh_data()` constructs empty `PathArgs` for every child
(line 520-524 of `node_entry.rs`). `PathPickerNode` needs the current
suffix in `data()` to know what directory to list. All existing node types
ignore `path_args` in `data()`, so this gap was invisible.


**Fix:** Modify `refresh_data()` to accept the prompt section (`&str`) and
parsed section (`&ParsedSection`). Pass the relevant suffix in `PathArgs`:
- **Matched child:** pass `parsed.tail` as suffix (everything after the
  matched ID).
- **Unmatched children:** pass the positional terms joined as suffix
  (the fuzzy filter text — for PathPickerNode, this is the path prefix
  being typed).

This is safe for existing nodes: `StaticListingNode`, `ModelNode`,
`ThinkingEnumNode`, `PersonaEnumNode` all ignore `path_args` in `data()`.

Call sites to update: `node_traverse()` (line 264) and any other caller
of `refresh_data()`.

### 9.2 Tree structure

/sandbox (StaticListingNode)
- columns: id (host_path), description (sandbox_path), mode (Mode), source (Source)
- fields: mode (string), source (string)
- Children:
  1. SandboxPathNode (listing — dynamic data)
     - data(): returns rows from combined_list(); id=host_path
     - header(): "Press enter to remove the path mapping"
     - children:
       - SandboxRemoveNode
         - data() row: "remove" | "Remove this mapping"
         - prepare(): confirm dialog (two-phase) accept / cancel
         - submit(): remove/exclude binding, re-merge, publish
  2. add (StaticListingNode)
     - data() row: {id: "add", desc: "Add a path mapping to the sandbox"}
     - children:
       - PathPickerNode (StaticListingNode)
       - rows: [{id: "ro", desc: "read only mapping"},
                {id: "rw", desc: "read-write mapping"}]
       - columns: [id="Path"]
       - children:
         - PathPickerNode (leaf — dynamic data)
           - data(): live host filesystem autocomplete
           - header(): self-documenting prompt
           - submit(): call add_ro/add_rw, re-merge, publish

Prompt examples:
- `/sandbox ` → root listing: all bindings + "add" row
- `/sandbox /home/user/project ` → leaf: selected binding, header shown
- `/sandbox add ` → mode selector: ro / rw
- `/sandbox add ro ` → path picker: filesystem listing
- `/sandbox add ro /home/gab ` → path picker: /home/ entries, filtered
- `/sandbox add ro /home/gabriel/project ` → Enter: adds the binding

### 9.3 SandboxPathNode

A single `SandboxPathNode` instance returns ALL binding rows from
`sandbox_bindings::combined_list()`. Each row:
- `id`: host path
- `description`: sandbox path (display column)
- `field("mode")`: "ro" or "rw"
- `field("source")`: "cfg" or "session"

The node holds `Rc<MenuShared>` for access to sandbox config state.

**`data()`:** Calls `sandbox_bindings::combined_list()`, maps each
`CombinedBind` to a `Row`. One row per binding. Returns the same `Rc`
when the list is unchanged (engine detects via `Rc::ptr_eq`).

**`header()`:** `vec!["Press enter to remove the path mapping"]`

**`prepare()`:** Returns `Prepared` (two-phase confirm). The engine shows
the confirm dialog. TODO: Can a message be shown at the accept / cancel stage?

**`submit()`:** Reads selected binding ID from `path_args.ids.last()`
(or fallback to `path_args.args.positional`). Calls
`sandbox_bindings::remove(sandbox_path)` (or `exclude` for cfg bindings).
Then re-merges and publishes:
```rust
let mut cfg = SandboxConfig::default();
sandbox_bindings::merge_into(&mut cfg);
tools::set_sandbox(Some(cfg));
```
Returns `Done(Some("sandbox: removed <path> (ok)"))`.

**Columns on the parent (root listing):**
```rust
vec![
    ColumnDef::new("id", "Sandbox Path"),
    ColumnDef::new("description", "Host Path"),
    ColumnDef::new("mode", "Mode"),
    ColumnDef::new("source", "Source"),
]
```

### 9.4 PathPickerNode

A dynamic leaf that returns filesystem path entries as data rows.
Configurable via a `PathScope` enum.

```rust
enum PathScope {
    /// Lists host filesystem (for adding sandbox bindings).
    HostOnly,
    /// Uses sandbox_list_dir when sandbox is active, falls back to host.
    /// (For future inline prompt completer reuse.)
    SandboxAware,
}
```

**Fields:**
- `scope: PathScope`
- `shared: Rc<MenuShared>` (for sandbox config access, if SandboxAware)
- `cache: RefCell<Option<(String, Instant, Rc<DataResponse>)>>`
  — keyed by deepest directory in suffix, 10s TTL

**`data(epoch, path_args)`:**
1. Extract suffix from `path_args.suffix` (or `path_args.args.positional
   .join(" ")` if suffix is empty).
2. Compute deepest complete directory from suffix:
   - `/home/gab` → deepest dir = `/home/` (last complete component)
   - `/home/gabriel/s` → deepest dir = `/home/gabriel/`
   - Empty / no `/` → list CWD or `/`
3. Check cache: if deepest dir matches AND < 10s elapsed → return cached `Rc`.
4. List the directory:
   - `HostOnly` → `std::fs::read_dir(deepest_dir)`
   - `SandboxAware` → `tools::sandbox_list_dir(deepest_dir)`, fallback to host
5. Map entries to rows: directories get trailing `/`, files as-is.
   Each row: `id = full_path`, description empty.
6. Cache the `Rc<DataResponse>` with current Instant and deepest dir.
7. Return the `Rc`.

**Path completion logic** (reuse `complete_path_prefix_impl` pattern):
```
for entry in read_dir(parent_dir):
    name = entry.name
    if name.starts_with(stem):
        yield if is_dir { format!("{}/", full_path) } else { full_path }
```

**`header()`:** `vec!["Type a path and press Tab to expand, Enter to select"]`

**`submit(path_args)`:**
1. Read selected path from `path_args.ids.last()` (the row ID = full path).
2. Strip trailing `/` if present (directories can be selected too).
3. Read mode from `path_args.ids`: search backwards for "ro" or "rw"
   (the parent listing row that was drilled into).
4. Read optional second positional (sandbox dest path) from
   `path_args.args.positional.get(1)`.
5. Call `sandbox_bindings::add_ro(host, sandbox)` or `add_rw(...)`.
6. Re-merge and publish:
   ```rust
   let mut cfg = SandboxConfig::default();
   sandbox_bindings::merge_into(&mut cfg);
   tools::set_sandbox(Some(cfg));
   ```
7. Return `Done(Some("sandbox: added <path> (ok)"))`.

**`columns()`:** `vec![ColumnDef::new("id", "Path")]`

### 9.5 Remap support

Optional second positional parameter after the host path:
- `/sandbox add ro /host/path /sandbox/dest` → remap
- `/sandbox add ro /host/path` → simple bind (host = sandbox)

PathPickerNode only provides autocomplete for the first positional (host
path). The second positional is typed manually by the user.

### 9.6 Files changed

| File | Change |
|------|--------|
| `src/v25_sandbox.rs` | **New** — SandboxPathNode, PathPickerNode, PathScope |
| `src/v25_menu.rs` | Add sandbox subtree to root menu builder |
| `crates/slashdot_menu/src/node_entry.rs` | Thread prompt suffix through `refresh_data()` |
| `src/slash_menu_bridge.rs` | Maybe: sandbox config state in MenuShared/LiveState |

### 9.7 Sandbox config state access

`submit()` needs to re-merge and publish the sandbox config. The sync
path is:
1. `sandbox_bindings::add_ro/add_rw/remove()` — sync, mutex-protected
2. Build a fresh `SandboxConfig`, call `merge_into()` — sync
3. `tools::set_sandbox(Some(cfg))` — sync, writes to global

This can be done directly from `submit()` (no AsyncOp needed).
No additional state on `MenuShared` is required — the sandbox bindings
module already owns the state.

### 9.8 Tests

| Test | What it validates |
|------|-------------------|
| SandboxPathNode rows from combined_list | Dynamic data from bindings |
| SandboxPathNode submit removes binding | Two-phase confirm + remove |
| SandboxPathNode columns render | Table with mode/source fields |
| PathPickerNode lists CWD on empty suffix | Default directory listing |
| PathPickerNode lists subdirectory | Suffix-based directory drill |
| PathPickerNode cache hit | Same Rc within 10s TTL |
| PathPickerNode cache miss after TTL | New Rc after 10s |
| PathPickerNode cache miss on level change | New Rc when suffix dir changes |
| PathPickerNode submit adds binding | add_ro/add_rw + config merge |
| PathPickerNode remap support | Second positional parameter |
| Mixed listing: bindings + add row | Root listing merges both children |
| Fuzzy filter on bindings | Filter narrows binding rows |
| Tab drill: add → mode → path | Full navigation flow |
| Root columns: sandbox_path, host, mode, source | Table rendering with all fields |
