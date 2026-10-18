//! `dsf`: a dark software factory that turns approved GitHub issues into
//! verified draft pull requests. See `docs/design.md`.

use clap::Parser;

/// Turns approved GitHub issues into verified draft pull requests.
#[derive(Parser)]
#[command(version, about)]
struct Cli {}

fn main() {
    Cli::parse();
}
