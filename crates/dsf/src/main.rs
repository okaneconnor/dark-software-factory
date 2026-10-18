use std::path::{Path, PathBuf};
use std::process::ExitCode;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use dsf_core::clamp::Caps;
use dsf_core::config::Config;
use dsf_core::workflow::{self, Workflow};

/// Turns approved GitHub issues into verified draft pull requests.
#[derive(Parser)]
#[command(version, about)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Check and print workflow files.
    #[command(subcommand)]
    Workflow(WorkflowCommand),
    /// Check the configuration file.
    #[command(subcommand)]
    Config(ConfigCommand),
}

#[derive(Subcommand)]
enum WorkflowCommand {
    /// Check a workflow file, such as a repo's .factory/workflow.toml.
    Validate {
        file: PathBuf,
        /// Apply this config's caps, as dsf would when running the repo.
        /// Without it, the default caps apply.
        #[arg(long, value_name = "FILE")]
        config: Option<PathBuf>,
    },
    /// Print the built-in workflow, a starting point for a repo's own.
    Default,
}

#[derive(Subcommand)]
enum ConfigCommand {
    /// Check a configuration file, such as /etc/dsf/config.toml.
    Validate { file: PathBuf },
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    let result = match cli.command {
        Command::Workflow(WorkflowCommand::Validate { file, config }) => {
            validate_workflow(&file, config.as_deref())
        }
        Command::Workflow(WorkflowCommand::Default) => {
            print!("{}", workflow::DEFAULT_TOML);
            Ok(())
        }
        Command::Config(ConfigCommand::Validate { file }) => validate_config(&file),
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("error: {error:#}");
            ExitCode::FAILURE
        }
    }
}

fn validate_workflow(file: &Path, config: Option<&Path>) -> Result<()> {
    let caps = match config {
        Some(path) => load_config(path)?.caps,
        None => Caps::default(),
    };
    let workflow = Workflow::parse(&read(file)?).with_context(|| file.display().to_string())?;
    let (workflow, adjustments) = workflow
        .clamp(&caps)
        .with_context(|| file.display().to_string())?;
    for adjustment in &adjustments {
        println!("note: {adjustment}");
    }
    println!(
        "{}: valid, {} stages, at most {} stage visits",
        file.display(),
        workflow.stages().len(),
        workflow.visit_budget()
    );
    Ok(())
}

fn validate_config(file: &Path) -> Result<()> {
    let config = load_config(file)?;
    let repos = config.repos.len();
    println!(
        "{}: valid, {repos} {}, {} {}, window {}-{} {}",
        file.display(),
        if repos == 1 { "repo" } else { "repos" },
        config.workers,
        if config.workers == 1 {
            "worker"
        } else {
            "workers"
        },
        config.window.start.strftime("%H:%M"),
        config.window.end.strftime("%H:%M"),
        config.timezone,
    );
    Ok(())
}

/// Parses a config file and resolves its time zone, which `dsf-core`
/// can't do because it never reads the tz database.
fn load_config(file: &Path) -> Result<Config> {
    let config = Config::parse(&read(file)?).with_context(|| file.display().to_string())?;
    jiff::tz::TimeZone::get(&config.timezone).with_context(|| {
        format!(
            "{}: unknown time zone `{}`",
            file.display(),
            config.timezone
        )
    })?;
    Ok(config)
}

fn read(file: &Path) -> Result<String> {
    std::fs::read_to_string(file).with_context(|| format!("reading {}", file.display()))
}
