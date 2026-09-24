# Slashdot state and async review

Issues with the current architecture
- Sync impl methods: Cannot directly execute async; AsyncOp indirection
  creates complexity
- Complex state access: MenuShared with Rc's, lots of clones, sync complexity
  Also source of bugs, e.g. model changes

## Idea
- Make menu_eval* and the paths down through the tree fully async.
  SlashDotNode handlers that might perform IO or access / update async
  settings all become async too: data(), prepare(), submit(), header(),
  complete_parameter().
- Pass all state down the chain: Make the menu polymorphic / generic over a
  context type, and pass that context type by `&mut` down eval_menu*. No more
  MenuShared. All trait handlers receive the `&mut app_ctx`, and can access
  app state that way. Instances are free to also have additional state in
  their structs - that is up to the implementation. But the app_ctx mechanism
  exists so direct by-ref access is easy.
