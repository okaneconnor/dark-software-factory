use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use std::time::Duration;

use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand, ValueEnum};
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
    /// Run one issue through the factory, or resume a run.
    Run(Box<RunArgs>),
    /// Check and print workflow files.
    #[command(subcommand)]
    Workflow(WorkflowCommand),
    /// Check the configuration file.
    #[command(subcommand)]
    Config(ConfigCommand),
    /// Run the egress proxy. dsf starts this in its proxy sidecar container.
    Proxy {
        #[arg(long, default_value = "0.0.0.0:3128")]
        listen: SocketAddr,
        /// A host name agents may reach over HTTPS. Repeat for each host.
        #[arg(long = "allow", required = true, value_name = "HOST")]
        allow: Vec<String>,
    },
}

#[derive(clap::Args)]
struct RunArgs {
    /// Work on a local repository, with no GitHub involved.
    #[arg(
        long,
        value_name = "REPO",
        required_unless_present = "resume",
        conflicts_with = "resume"
    )]
    local: Option<PathBuf>,
    /// The issue as markdown: a `# Title` line, then the body.
    #[arg(long, value_name = "FILE", requires = "local")]
    issue_file: Option<PathBuf>,
    #[arg(long, default_value_t = 1, value_name = "N")]
    issue_number: u64,
    /// Where to push the result branch. Defaults to the local repository.
    #[arg(long, value_name = "URL")]
    publish_to: Option<String>,
    /// Resume a suspended or interrupted run.
    #[arg(long, value_name = "RUN")]
    resume: Option<i64>,
    /// Approve the plan of a run waiting at its gate.
    #[arg(long, requires = "resume")]
    approve: bool,
    /// Make gate stages wait for approval.
    #[arg(long)]
    require_approval: bool,
    /// Where runs and their database live.
    #[arg(long, value_name = "DIR")]
    state_dir: Option<PathBuf>,
    /// The image agent and verify stages run in.
    #[arg(long, default_value = "dsf-worker:dev", value_name = "IMAGE")]
    image: String,
    #[arg(long, default_value = "claude-sonnet-5-5")]
    model: String,
    /// How agents reach the internet: through the egress proxy, or not at all.
    #[arg(long, value_enum, default_value = "proxy")]
    egress: EgressArg,
    /// The dsf image the proxy sidecar runs from.
    #[arg(long, default_value = "dsf:dev", value_name = "IMAGE")]
    dsf_image: String,
    /// The Claude OAuth token, from `claude setup-token`.
    #[arg(long, value_name = "FILE")]
    token_file: Option<PathBuf>,
    /// Prefix for this dsf's container names and labels.
    #[arg(long, default_value = "dsf", hide = true)]
    instance: String,
    /// Longest an agent stage may run.
    #[arg(long, default_value = "45m", value_parser = parse_duration)]
    agent_timeout: Duration,
}

#[derive(Clone, Copy, PartialEq, Eq, ValueEnum)]
enum EgressArg {
    Proxy,
    None,
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
        Command::Run(args) => return run_issue(&args),
        Command::Workflow(WorkflowCommand::Validate { file, config }) => {
            validate_workflow(&file, config.as_deref())
        }
        Command::Workflow(WorkflowCommand::Default) => {
            print!("{}", workflow::DEFAULT_TOML);
            Ok(())
        }
        Command::Config(ConfigCommand::Validate { file }) => validate_config(&file),
        Command::Proxy { listen, allow } => run_proxy(listen, &allow),
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

/// Exit codes for `dsf run`, so scripts can tell outcomes apart.
const EXIT_BLOCKED: u8 = 2;
const EXIT_SUSPENDED: u8 = 3;
const EXIT_PUBLISH_FAILED: u8 = 4;

fn run_issue(args: &RunArgs) -> ExitCode {
    let outcome = tokio::runtime::Runtime::new()
        .map_err(anyhow::Error::from)
        .and_then(|runtime| runtime.block_on(run_issue_async(args)));
    match outcome {
        Ok(dsf::engine::RunEnd::Published {
            run_id,
            branch,
            sha,
        }) => {
            println!("run {run_id}: published {branch} at {sha}");
            ExitCode::SUCCESS
        }
        Ok(dsf::engine::RunEnd::Blocked { run_id, reason }) => {
            println!("run {run_id}: blocked: {reason}");
            ExitCode::from(EXIT_BLOCKED)
        }
        Ok(dsf::engine::RunEnd::Suspended {
            run_id,
            detail,
            resume_after,
            ..
        }) => {
            match resume_after {
                Some(at) => println!("run {run_id}: suspended until {at}: {detail}"),
                None => println!("run {run_id}: suspended: {detail}"),
            }
            println!("resume with: dsf run --resume {run_id}");
            ExitCode::from(EXIT_SUSPENDED)
        }
        Ok(dsf::engine::RunEnd::PublishFailed { run_id, reason }) => {
            println!("run {run_id}: {reason}");
            ExitCode::from(EXIT_PUBLISH_FAILED)
        }
        Err(error) => {
            eprintln!("error: {error:#}");
            ExitCode::FAILURE
        }
    }
}

async fn run_issue_async(args: &RunArgs) -> Result<dsf::engine::RunEnd> {
    use dsf::engine::{Config, Engine, ResumeOptions};

    let home = std::env::var_os("HOME").map(PathBuf::from);
    let state_dir = state_dir(args, home.as_deref())?;
    let token = claude_token(args, home.as_deref())?;
    let sandbox = dsf::sandbox::Sandbox::connect()?;
    sandbox.ping().await?;
    let egress = egress(&sandbox, args).await?;
    let config = Config {
        instance: args.instance.clone(),
        state_dir: state_dir.clone(),
        worker_image: args.image.clone(),
        model: args.model.clone(),
        max_turns: 80,
        agent_timeout: args.agent_timeout,
        limits: dsf::sandbox::Limits::default(),
        egress,
        caps: Caps::default(),
        extra_protected: Vec::new(),
        claude_token: token,
        require_approval: args.require_approval,
    };
    let store = dsf::store::Store::open(&state_dir.join("dsf.sqlite"))?;
    let report = Box::new(|line: &str| println!("{line}"));
    let engine = Engine::new(config, store, sandbox, dsf::hostgit::Git::locate()?, report)?;
    match args.resume {
        Some(run_id) => {
            engine
                .resume(
                    run_id,
                    ResumeOptions {
                        approve: args.approve,
                    },
                )
                .await
        }
        None => engine.start(&local_issue(args)?).await,
    }
}

fn state_dir(args: &RunArgs, home: Option<&Path>) -> Result<PathBuf> {
    let dir = match (&args.state_dir, home) {
        (Some(dir), _) => dir.clone(),
        (None, Some(home)) => home.join(".local/state/dsf"),
        (None, None) => bail!("set --state-dir or HOME"),
    };
    std::fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
    Ok(dir.canonicalize()?)
}

fn claude_token(args: &RunArgs, home: Option<&Path>) -> Result<secrecy::SecretString> {
    match (&args.token_file, home, args.egress) {
        (Some(file), _, _) => read_token(file),
        (None, Some(home), EgressArg::Proxy) => {
            read_token(&home.join(".config/dsf/claude-oauth-token"))
        }
        // Offline runs use fake agents, which need no token.
        (None, _, EgressArg::None) => Ok(secrecy::SecretString::from("offline-no-token")),
        (None, None, EgressArg::Proxy) => bail!("set --token-file or HOME"),
    }
}

/// Sets up the agents' network and the proxy sidecar, unless offline.
async fn egress(sandbox: &dsf::sandbox::Sandbox, args: &RunArgs) -> Result<dsf::engine::Egress> {
    const NETWORK: &str = "dsf-agents";
    const PROXY: &str = "dsf-proxy";
    if args.egress == EgressArg::None {
        return Ok(dsf::engine::Egress::None);
    }
    sandbox.ensure_internal_network(NETWORK).await?;
    let allow: Vec<String> = dsf_core::config::BUILTIN_EGRESS
        .iter()
        .map(ToString::to_string)
        .collect();
    sandbox
        .ensure_proxy(&dsf::sandbox::ProxySidecar {
            name: PROXY,
            image: &args.dsf_image,
            internal_network: NETWORK,
            allow: &allow,
        })
        .await?;
    Ok(dsf::engine::Egress::Proxy {
        network: NETWORK.to_owned(),
        proxy_url: format!("http://{PROXY}:{}", dsf::sandbox::PROXY_PORT),
    })
}

/// The issue for a `--local` run: the repository and an issue file.
fn local_issue(args: &RunArgs) -> Result<dsf::engine::Issue> {
    let (Some(repo), Some(issue_file)) = (&args.local, &args.issue_file) else {
        bail!("--local needs --issue-file");
    };
    let repo = repo
        .canonicalize()
        .with_context(|| format!("{} not found", repo.display()))?;
    let (title, body) = parse_issue(&read(issue_file)?)?;
    let url = repo.display().to_string();
    let name = repo
        .file_name()
        .map_or_else(|| "repo".to_owned(), |n| n.to_string_lossy().into_owned());
    Ok(dsf::engine::Issue {
        repo: format!("local/{name}"),
        number: args.issue_number,
        title,
        body,
        author: "local".to_owned(),
        clone_url: url.clone(),
        publish_url: args.publish_to.clone().unwrap_or(url),
    })
}

fn read_token(file: &Path) -> Result<secrecy::SecretString> {
    let token = std::fs::read_to_string(file)
        .with_context(|| format!("reading the token from {}", file.display()))?;
    let token = token.trim();
    if token.is_empty() {
        bail!("{} is empty", file.display());
    }
    Ok(secrecy::SecretString::from(token.to_owned()))
}

/// `# Title` on the first line, then the body.
fn parse_issue(text: &str) -> Result<(String, String)> {
    let mut lines = text.lines();
    let title = lines
        .next()
        .and_then(|line| line.strip_prefix("# "))
        .map(str::trim)
        .filter(|title| !title.is_empty())
        .context("the issue file must start with a `# Title` line")?;
    let body = lines.collect::<Vec<_>>().join("\n").trim().to_owned();
    Ok((title.to_owned(), body))
}

fn parse_duration(text: &str) -> std::result::Result<Duration, String> {
    dsf_core::duration::parse(text).map_err(|error| error.to_string())
}

fn run_proxy(listen: SocketAddr, allow: &[String]) -> Result<()> {
    let runtime = tokio::runtime::Runtime::new()?;
    runtime.block_on(async {
        let listener = tokio::net::TcpListener::bind(listen)
            .await
            .with_context(|| format!("listening on {listen}"))?;
        println!("proxy listening on {listen}, allowing {}", allow.join(", "));
        dsf::proxy::serve(listener, dsf::proxy::Policy::new(allow)).await
    })
}

fn read(file: &Path) -> Result<String> {
    std::fs::read_to_string(file).with_context(|| format!("reading {}", file.display()))
}
