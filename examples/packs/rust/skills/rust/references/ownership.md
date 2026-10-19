# Ownership and data

- Borrow by default. Clone only when the code genuinely needs its own copy:
  storing it, sending it to another task, or outliving the borrow.
- When a `clone()` exists only to satisfy the borrow checker, restructure
  instead: shorten the borrow's scope, split the struct, or pass the piece
  that's needed.
- `Arc` shares ownership across threads or tasks; `Rc` only within one
  thread. Clone the `Arc`, not what it points to.
- Shared mutable state: prefer passing ownership or messages (channels). When
  a lock is the right tool, keep the critical section short and never call
  out (IO, callbacks, `.await`) while holding it.
- Iterators over index loops. Chain `map`, `filter` and `collect` when it
  reads clearly; use a plain `for` loop when the body has side effects or
  early exits.
- Collect into the type you need (`Vec`, `HashMap`, `String`) directly, and
  pre-size with `with_capacity` when the length is known.
- `BTreeMap` and `BTreeSet` when order matters or output must be stable
  (snapshots, files); `HashMap` otherwise.
- `Cow<'_, str>` is for a function that usually returns its input unchanged;
  don't use it just to avoid an occasional allocation.
- Lifetimes: let elision do the work. If a struct needs a lifetime parameter,
  ask whether it should own its data instead.
