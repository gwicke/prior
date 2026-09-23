# Slashdot state and async review

Issues with the current architecture
- Sync impl methods: Cannot directly execute async; AsyncOp indirection
  creates complexity
- Complex state access: MenuShared with Rc's, lots of clones, sync complexity
