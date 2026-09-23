# Sandbox design & implementation


## Tree structure

/sandbox (StaticListingNode)
- row: id="sandbox", description="Manage sandbox path mappings"
- columns: id (host_path), description (sandbox_path), mode (Mode), source (Source)
- fields: mode (string), source (string)
- Children:
  1. "add" (StaticListingNode) - added first, so it shows on top in default
     listing
     - data() row: {id: "add", desc: "Add a path mapping to the sandbox"}
       This does not match the fields returned by paths exactly, but will
       render in the src / dst columns. Rendering is tolerant wrt missing
       fields other than ID.
     - children:
       - AddSandboxMappingNode (leaf)
         - rows: [{id: "ro", desc: "read only mapping"},
                  {id: "rw", desc: "read-write mapping"}]
         - header(): "Specify host path and optionally sandbox path"
         - complete_parameter() for filesystem completion for both source and
           destination arguments (see ./arg_and_modifier_autocomplete.md)
         - submit() takes args, adds mapping
  2. `host_path` SandboxMappingNode (dynamic mapping data, action listing)
     - data(): id=host_path, description=sandbox_path, mode (ro/rw), source
       (cfg/session)
     - header(): Information about the mapping src/dst/mode/source
     - children:
       - "remove" SandboxRemoveNode
         - data() row: "remove" | "Remove this mapping"
         - prepare(): confirm dialog (two-phase) accept / cancel
         - submit(): remove/exclude binding, re-merge, publish
       - potentially later: editing of mappings - ro/rw switching, source swapping etc

Prompt examples:
- `/sandbox ` → root listing: all bindings + "add" row
- `/sandbox /home/user/project ` → leaf: selected binding, header shown
- `/sandbox add ` → mode selector: ro / rw
- `/sandbox add ro ` → header() shown
- `/sandbox add ro /home/gab` → path picker: /home/ entries, filtered
- `/sandbox add ro /home/gabriel/project` → Enter: adds the binding

## AddSandboxMappingNode details
- Completion sources: We have existing implementations of host-level and
  sandbox-level path completion. If they are not yet stand-alone extract these
  into a general module, and call that module.
- Incremental results:
  FS completion drills level-by-level; see ./arg_and_modifier_autocomplete.md
  for how this works in detail. After a tab-complete, the handler is called
  again, and if the new path is a directory (ends in `/`), will list that
  directory level internally. If there are new chilren, it shall return a new
  set of candidates, including the directory itself. If the directory is empty
  (or the user completed to a file), then there is no non-exact match in the
  completion candidates, and the handler returns an empty vec.
  Column labels for FS completion: "Host path" / "Sandbox path". For
  description, ideally set an ISO mtime timestamp, followed by size.
