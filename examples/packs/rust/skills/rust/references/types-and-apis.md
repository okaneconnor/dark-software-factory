# Types and APIs

## Make invalid values hard to build

- Wrap values that have rules in a newtype with a constructor that checks
  them, and keep the field private:

  ```rust
  pub struct RepoSlug { owner: String, name: String }

  impl FromStr for RepoSlug {
      type Err = String;
      fn from_str(text: &str) -> Result<Self, Self::Err> { /* validate once */ }
  }
  ```

  Code that receives a `RepoSlug` never has to check it again.
- Use an enum for a closed set of choices instead of strings or a cluster of
  `bool`s. Two `bool` parameters side by side are a bug waiting to happen;
  name the choice with an enum or an options struct.
- Prefer `Option<T>` fields over sentinel values (`""`, `0`, `-1`).

## Signatures

- Take the most general borrowed form: `&str` over `&String`, `&[T]` over
  `&Vec<T>`, `&Path` over `&PathBuf`, `impl AsRef<Path>` when callers vary.
- Return owned values; return borrowed ones only when they live in `self`.
- Group more than three or four parameters into a struct.
- Mark functions whose result must be used with `#[must_use]`, especially
  builders that return `Self`.

## Naming

- `as_*` is a cheap view, `to_*` makes a new value, `into_*` consumes `self`.
- Getters are named after the field, without `get_`.
- Conversions implement `From`/`TryFrom`/`FromStr` rather than ad hoc
  functions, so `?` and `.into()` work.
- Types and traits are `UpperCamelCase`, everything else `snake_case`;
  acronyms are words (`HttpClient`, not `HTTPClient`).

## Visibility

- Keep items private by default; `pub(crate)` before `pub`.
- A public enum that may grow gets `#[non_exhaustive]` in a library crate.
- Don't expose a dependency's types in a public API unless callers are meant
  to use that dependency too.

## Traits and generics

- Reach for a trait when there are, or soon will be, several real
  implementations, such as a real client and a test fake. Not before.
- Prefer generics with trait bounds for speed and clarity; use `dyn Trait`
  when the set of types is open at runtime or generics would spread
  everywhere.
- Derive the standard traits that make sense (`Debug`, `Clone`, `PartialEq`,
  `Eq`, `Hash`, `Default`). Every public type should at least be `Debug`.
