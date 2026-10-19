# Documentation and comments

- **Public items** get a doc comment (`///`) that says what the item is for
  and what it promises: inputs it accepts, what it returns, errors, panics if
  any, and anything surprising. Start with a one-line summary.
- **Modules** get a `//!` comment saying what they're responsible for and how
  they fit into the crate, when that isn't obvious from the name.
- **Comments explain why**, not what: the reason for a non-obvious choice,
  an invariant, a trade-off, a link to the issue or spec. If a comment only
  restates the code, delete it or improve the names.
- **Keep them true.** A change that alters behaviour updates the comments and
  docs it makes wrong, in the same change.
- Examples in doc comments are tests: keep them compiling, or mark them
  `ignore` with a reason.
- Write in plain words for the reader who arrives next year. Prefer
  "Returns `None` if the issue has no plan yet" to restating the type
  signature.
