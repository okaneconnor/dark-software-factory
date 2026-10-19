# Async

- **Never block the runtime.** File and process work that can take time,
  blocking libraries and heavy computation go to
  `tokio::task::spawn_blocking`, or use the async equivalent (`tokio::fs`,
  `tokio::process`).
- **Locks.** Never hold a `std::sync::Mutex` (or `RwLock`) guard across an
  `.await`. Scope the guard so it drops first, or use `tokio::sync::Mutex`
  when a lock truly must span an await.
- **Bound every wait.** Network calls, process runs and waits on other tasks
  get a timeout (`tokio::time::timeout`, or the client's own) and a clear
  error when it expires.
- **Tasks are owned.** Don't fire and forget. Keep the `JoinHandle`, or use
  `JoinSet` or `FuturesUnordered`, and handle each task's result, including
  panics and cancellation.
- **Cancellation.** Dropping a future cancels it at its next `.await`. Write
  code so that's safe: no half-written state across an await without a way
  to recover, and be careful which branches of `tokio::select!` can lose
  work.
- **Shutdown.** Long-running work listens for a shutdown signal and finishes
  or checkpoints what it's doing; nothing relies on being killed cleanly.
- **Don't go async without a reason.** CPU-bound or purely in-memory code
  stays synchronous; call it from async code as an ordinary function.
- Pass time in (a clock argument or trait) instead of reading it inside
  logic, so tests control it.
