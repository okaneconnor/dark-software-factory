# Errors

## Libraries and applications

- **A library** (or a crate other crates call, such as a `-core` crate)
  defines its own error type, usually an enum with `thiserror`, so callers
  can match on what went wrong:

  ```rust
  #[derive(Debug, thiserror::Error)]
  pub enum ConfigError {
      #[error("not a valid config file: {0}")]
      Toml(#[from] toml::de::Error),
      #[error("`{field}`: {message}")]
      Invalid { field: String, message: String },
  }
  ```

- **An application** (a binary, or the edge of one) uses `anyhow::Result`
  and adds context where an error crosses a boundary: a file, a process, a
  network call, a user's input.

  ```rust
  let text = std::fs::read_to_string(&path)
      .with_context(|| format!("reading {}", path.display()))?;
  ```

## Messages

- Say what was being attempted and with what: `reading /etc/dsf/config.toml`,
  not `failed` or `error occurred`.
- Lowercase, no trailing full stop: messages are joined into chains like
  `reading config.toml: permission denied`.
- Don't repeat what the source error already says, and don't wrap an error
  twice at the same level.
- Show the whole chain to people: `{error:#}` with `anyhow`.

## Propagation

- Use `?`. Convert with `#[from]` or `map_err`, keeping the original as the
  source so nothing is lost.
- Never drop an error silently. `let _ = fallible();` is for results that
  truly don't matter, with a comment saying why.
- Don't log an error and also return it: whoever handles it decides.
- `Option` is for "absent is normal"; `Result` is for "this went wrong".

## Panics

- A panic means a bug in this program, never bad input, missing files, the
  network or another process.
- No `unwrap()` or `expect()` outside tests. When a value really can't be
  missing, restructure so the type system knows (a non-empty type, a match
  that covers every case) rather than asserting it.
- `unreachable!("why")` is acceptable where the reason is local and obvious.
  Indexing (`v[i]`) is a panic too: prefer `get`, iterators or pattern
  matching when the index comes from data.
- In tests, `unwrap()` and `expect()` are fine: a panic is a failed test.
