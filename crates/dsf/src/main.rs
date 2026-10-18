use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::str::FromStr;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand, ValueEnum};
use dsf::engine::RunEnd;
use dsf::factory::Outcome;
use dsf_core::clamp::Caps;
use dsf_core::config::{Config, RepoSlug};
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
    /// One pass of the factory: recover from any crash, look at every
    /// configured repo, and work through the queue while the window is
    /// open. A systemd timer runs this every few minutes.
    Tick(Box<WorkerArgs>),
    /// Show the queue, the runs in progress and the last week's results.
    Status(StateArgs),
    /// Stop claiming new work. Runs already going carry on.
    Pause {
        /// Why, shown by `dsf status`.
        #[arg(default_value = "paused by the operator")]
        reason: String,
        #[command(flatten)]
        state: StateArgs,
    },
    /// Let claiming start again after a pause.
    Unpause(StateArgs),
    /// Check and print workflow files.
    #[command(subcommand)]
    Workflow(WorkflowCommand),
    /// Check the configuration file.
    #[command(subcommand)]
    Config(ConfigCommand),
    /// Manage the factory's labels in a repository.
    #[command(subcommand)]
    Labels(LabelsCommand),
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
    /// The GitHub issue to work on, e.g. octocat/hello#12. Its repository
    /// must be in the config.
    #[arg(
        value_name = "OWNER/REPO#N",
        required_unless_present_any = ["local", "resume"],
        conflicts_with_all = ["local", "resume"]
    )]
    issue: Option<IssueRef>,
    /// Work on a local repository, with no GitHub involved.
    #[arg(long, value_name = "REPO", conflicts_with = "resume")]
    local: Option<PathBuf>,
    /// The issue as markdown: a `# Title` line, then the body.
    #[arg(long, value_name = "FILE", requires = "local")]
    issue_file: Option<PathBuf>,
    #[arg(long, default_value_t = 1, value_name = "N")]
    issue_number: u64,
    /// Where to push the result branch. Defaults to the local repository.
    #[arg(long, value_name = "URL")]
    publish_to: Option<String>,
    /// Resume a suspended or interrupted local run. GitHub runs continue
    /// with `dsf run OWNER/REPO#N`.
    #[arg(long, value_name = "RUN")]
    resume: Option<i64>,
    /// Approve the plan of a run waiting at its gate.
    #[arg(long, requires = "resume")]
    approve: bool,
    /// Make gate stages wait for approval.
    #[arg(long, requires = "local")]
    require_approval: bool,
    #[command(flatten)]
    worker: WorkerArgs,
}

/// What running stages needs, for `dsf run` and `dsf tick`.
#[derive(clap::Args)]
struct WorkerArgs {
    /// Where runs and their database live. Defaults to the config's
    /// `state_dir` for GitHub runs, `~/.local/state/dsf` for local ones.
    #[arg(long, value_name = "DIR")]
    state_dir: Option<PathBuf>,
    /// The image agent and verify stages run in. Defaults to the config's
    /// `images.worker` for GitHub runs, `dsf-worker:dev` for local ones.
    #[arg(long, value_name = "IMAGE")]
    image: Option<String>,
    #[arg(long, default_value = "claude-sonnet-5-5")]
    model: String,
    /// How agents reach the internet: through the egress proxy, or not at all.
    #[arg(long, value_enum, default_value = "proxy")]
    egress: EgressArg,
    /// The dsf image the proxy sidecar runs from.
    #[arg(long, default_value = "dsf:dev", value_name = "IMAGE")]
    dsf_image: String,
    /// The Claude OAuth token, from `claude setup-token`. Defaults to
    /// `$CREDENTIALS_DIRECTORY/claude-oauth-token`, then
    /// `~/.config/dsf/claude-oauth-token`.
    #[arg(long, value_name = "FILE")]
    token_file: Option<PathBuf>,
    #[command(flatten)]
    github: GitHubArgs,
    /// Prefix for this dsf's container names and labels.
    #[arg(long, default_value = "dsf", hide = true)]
    instance: String,
    /// Longest an agent stage may run.
    #[arg(long, default_value = "45m", value_parser = parse_duration)]
    agent_timeout: Duration,
    /// Decide as if it were this time, e.g. 2026-10-19T23:30:00Z. For
    /// trying out the window and backoff rules.
    #[arg(long, value_name = "TIME", hide = true)]
    now: Option<jiff::Timestamp>,
}

/// Where the run state is, for commands that only read or flag it.
#[derive(clap::Args)]
struct StateArgs {
    /// The config file. Defaults to `$DSF_CONFIG`, then
    /// `~/.config/dsf/config.toml` if it exists, then `/etc/dsf/config.toml`.
    #[arg(long, value_name = "FILE")]
    config: Option<PathBuf>,
    /// Overrides the config's `state_dir`.
    #[arg(long, value_name = "DIR")]
    state_dir: Option<PathBuf>,
}

/// How to reach GitHub, for commands that do.
#[derive(clap::Args)]
struct GitHubArgs {
    /// The config file. Defaults to `$DSF_CONFIG`, then
    /// `~/.config/dsf/config.toml` if it exists, then `/etc/dsf/config.toml`.
    #[arg(long, value_name = "FILE")]
    config: Option<PathBuf>,
    /// The GitHub App's private key. Defaults to
    /// `$CREDENTIALS_DIRECTORY/github-app.pem`, then
    /// `~/.config/dsf/github-app.pem`.
    #[arg(long, value_name = "FILE")]
    github_key: Option<PathBuf>,
    #[arg(long, default_value = dsf::github::API, hide = true)]
    github_api: String,
    #[arg(long, default_value = dsf::factory::GIT_BASE, hide = true)]
    git_base: String,
}

/// `owner/repo#N`.
#[derive(Clone)]
struct IssueRef {
    repo: RepoSlug,
    number: u64,
}

impl FromStr for IssueRef {
    type Err = String;

    fn from_str(text: &str) -> Result<Self, Self::Err> {
        let (repo, number) = text
            .split_once('#')
            .ok_or_else(|| format!("`{text}` is not owner/repo#N"))?;
        let number = number
            .parse::<u64>()
            .ok()
            .filter(|n| *n > 0)
            .ok_or_else(|| format!("`{number}` is not an issue number"))?;
        Ok(Self {
            repo: repo.parse()?,
            number,
        })
    }
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

#[derive(Subcommand)]
enum LabelsCommand {
    /// Create the factory: labels in a configured repository, and put
    /// right any that differ. Safe to run again.
    Init {
        #[arg(value_name = "OWNER/REPO")]
        repo: RepoSlug,
        #[command(flatten)]
        github: GitHubArgs,
    },
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    let result = match cli.command {
        Command::Run(args) => return run_issue(&args),
        Command::Tick(args) => tick(&args),
        Command::Status(state) => status(&state),
        Command::Pause { reason, state } => set_paused(&state, Some(&reason)),
        Command::Unpause(state) => set_paused(&state, None),
        Command::Workflow(WorkflowCommand::Validate { file, config }) => {
            validate_workflow(&file, config.as_deref())
        }
        Command::Workflow(WorkflowCommand::Default) => {
            print!("{}", workflow::DEFAULT_TOML);
            Ok(())
        }
        Command::Config(ConfigCommand::Validate { file }) => validate_config(&file),
        Command::Labels(LabelsCommand::Init { repo, github }) => init_labels(&repo, &github),
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
const EXIT_SKIPPED: u8 = 5;

fn run_issue(args: &RunArgs) -> ExitCode {
    let outcome = tokio::runtime::Runtime::new()
        .map_err(anyhow::Error::from)
        .and_then(|runtime| runtime.block_on(run_issue_async(args)));
    match outcome {
        Ok(Outcome::Ended(end)) => report_end(&end),
        Ok(Outcome::Skipped(reason)) => {
            println!("skipped: {reason}");
            ExitCode::from(EXIT_SKIPPED)
        }
        Ok(Outcome::AwaitingApproval { run_id }) => {
            println!("run {run_id}: waiting for factory:plan-approved on the issue");
            ExitCode::from(EXIT_SUSPENDED)
        }
        Err(error) => {
            eprintln!("error: {error:#}");
            ExitCode::FAILURE
        }
    }
}

fn report_end(end: &RunEnd) -> ExitCode {
    match end {
        RunEnd::Published {
            run_id,
            branch,
            sha,
            pull_request,
        } => {
            println!("run {run_id}: published {branch} at {sha}");
            if let Some(pull_request) = pull_request {
                println!("draft pull request: {}", pull_request.url);
            }
            ExitCode::SUCCESS
        }
        RunEnd::Blocked { run_id, reason } => {
            println!("run {run_id}: blocked: {reason}");
            ExitCode::from(EXIT_BLOCKED)
        }
        RunEnd::Suspended {
            run_id,
            detail,
            resume_after,
            ..
        } => {
            match resume_after {
                Some(at) => println!("run {run_id}: suspended until {at}: {detail}"),
                None => println!("run {run_id}: suspended: {detail}"),
            }
            ExitCode::from(EXIT_SUSPENDED)
        }
        RunEnd::PublishFailed { run_id, reason } => {
            println!("run {run_id}: {reason}");
            ExitCode::from(EXIT_PUBLISH_FAILED)
        }
    }
}

async fn run_issue_async(args: &RunArgs) -> Result<Outcome> {
    use dsf::engine::{Config, Engine, ResumeOptions};

    if let Some(target) = &args.issue {
        return run_github_issue(args, target).await;
    }

    let worker = &args.worker;
    let home = std::env::var_os("HOME").map(PathBuf::from);
    let state_dir = state_dir(worker.state_dir.as_deref(), home.as_deref(), None)?;
    let _lock = dsf::rundir::StateLock::acquire(&state_dir)?;
    let token = claude_token(worker, home.as_deref())?;
    let sandbox = dsf::sandbox::Sandbox::connect()?;
    sandbox.ping().await?;
    let builtin: Vec<String> = dsf_core::config::BUILTIN_EGRESS
        .iter()
        .map(ToString::to_string)
        .collect();
    let egress = egress(&sandbox, worker, &builtin).await?;
    let config = Config {
        instance: worker.instance.clone(),
        state_dir: state_dir.clone(),
        worker_image: worker
            .image
            .clone()
            .unwrap_or_else(|| "dsf-worker:dev".to_owned()),
        model: worker.model.clone(),
        max_turns: 80,
        agent_timeout: worker.agent_timeout,
        limits: dsf::sandbox::Limits::default(),
        egress,
        caps: Caps::default(),
        extra_protected: Vec::new(),
        weekly_usage_cap: dsf_core::config::DEFAULT_WEEKLY_USAGE_CAP,
        claude_token: token,
    };
    let store = dsf::store::Store::open(&state_dir.join("dsf.sqlite"))?;
    let report = Box::new(|line: &str| println!("{line}"));
    let engine = Engine::new(config, store, sandbox, dsf::hostgit::Git::locate()?, report)?;
    let end = match args.resume {
        Some(run_id) => {
            if !engine.stored_run(run_id)?.repo.starts_with("local/") {
                bail!("run {run_id} is a GitHub run; continue it with `dsf run OWNER/REPO#N`");
            }
            engine
                .resume(
                    run_id,
                    ResumeOptions {
                        approve: args.approve,
                    },
                )
                .await?
        }
        None => engine.start(&local_issue(args)?).await?,
    };
    if let RunEnd::Suspended { run_id, .. } = &end {
        println!("resume with: dsf run --resume {run_id}");
    }
    Ok(Outcome::Ended(end))
}

/// One GitHub issue, from the config's repos, as the GitHub App.
async fn run_github_issue(args: &RunArgs, target: &IssueRef) -> Result<Outcome> {
    let home = std::env::var_os("HOME").map(PathBuf::from);
    let (config, github) = connect_github(&args.worker.github, home.as_deref())?;
    let state_dir = state_dir(
        args.worker.state_dir.as_deref(),
        home.as_deref(),
        Some(&config.state_dir),
    )?;
    let _lock = dsf::rundir::StateLock::acquire(&state_dir)?;
    let factory = github_factory(&args.worker, config, github, &state_dir, home.as_deref()).await?;
    factory.work_on(&target.repo, target.number).await
}

/// The factory as the config's GitHub App, with stages in hardened
/// containers behind the egress proxy. The caller holds the state lock.
async fn github_factory(
    worker: &WorkerArgs,
    config: Config,
    github: Arc<dsf::github::GitHub>,
    state_dir: &Path,
    home: Option<&Path>,
) -> Result<dsf::factory::Factory> {
    use dsf::engine::{Config as EngineConfig, Engine};

    let token = claude_token(worker, home)?;
    let sandbox = dsf::sandbox::Sandbox::connect()?;
    sandbox.ping().await?;
    let egress = egress(&sandbox, worker, &config.egress).await?;
    let engine_config = EngineConfig {
        instance: worker.instance.clone(),
        state_dir: state_dir.to_owned(),
        worker_image: worker
            .image
            .clone()
            .unwrap_or_else(|| config.worker_image.clone()),
        model: worker.model.clone(),
        max_turns: 80,
        agent_timeout: worker.agent_timeout,
        limits: dsf::sandbox::Limits::default(),
        egress,
        caps: config.caps,
        extra_protected: config.protect_extra.clone(),
        weekly_usage_cap: config.weekly_usage_cap,
        claude_token: token,
    };
    let store = dsf::store::Store::open(&state_dir.join("dsf.sqlite"))?;
    let report = Box::new(|line: &str| println!("{line}"));
    let engine = Engine::new(
        engine_config,
        store,
        sandbox,
        dsf::hostgit::Git::locate()?,
        report,
    )?
    .with_forge(Box::new(dsf::factory::GitHubForge::new(github.clone())));
    let mut factory = dsf::factory::Factory::new(engine, github, config, &worker.github.git_base);
    if let Some(now) = worker.now {
        factory = factory.with_clock(Arc::new(move || now));
    }
    Ok(factory)
}

fn tick(args: &WorkerArgs) -> Result<()> {
    let home = std::env::var_os("HOME").map(PathBuf::from);
    let (config, github) = connect_github(&args.github, home.as_deref())?;
    let state_dir = state_dir(
        args.state_dir.as_deref(),
        home.as_deref(),
        Some(&config.state_dir),
    )?;
    // A systemd timer won't start a tick while one is running, but a
    // `dsf run` might be: then this tick has nothing to do.
    let Some(_lock) = dsf::rundir::StateLock::try_acquire(&state_dir)? else {
        println!(
            "another dsf is working in {}; nothing to do",
            state_dir.display()
        );
        return Ok(());
    };
    let tz = jiff::tz::TimeZone::get(&config.timezone)?;
    let runtime = tokio::runtime::Runtime::new()?;
    let record = runtime.block_on(async {
        let factory = github_factory(args, config, github, &state_dir, home.as_deref()).await?;
        dsf::scheduler::Scheduler::new(factory, tz).tick().await
    })?;
    for note in &record.notes {
        println!("{note}");
    }
    if record.notes.is_empty() {
        println!("nothing to do");
    }
    Ok(())
}

fn status(args: &StateArgs) -> Result<()> {
    let (config, store) = open_state(args)?;
    let tz = jiff::tz::TimeZone::get(&config.timezone)?;
    let board = dsf::status::Board::load(&store, jiff::Timestamp::now())?;
    print!("{}", board.render(&config, &tz));
    Ok(())
}

fn set_paused(args: &StateArgs, reason: Option<&str>) -> Result<()> {
    let (_, mut store) = open_state(args)?;
    if let Some(reason) = reason {
        store.kv_set(dsf::scheduler::PAUSED, reason)?;
        println!("paused: no new work starts until `dsf unpause`; runs already going carry on");
    } else {
        store.kv_delete(dsf::scheduler::PAUSED)?;
        println!("claiming allowed again from the next tick");
    }
    Ok(())
}

/// The config and the run database, for commands that need no secrets.
fn open_state(args: &StateArgs) -> Result<(Config, dsf::store::Store)> {
    let home = std::env::var_os("HOME").map(PathBuf::from);
    let config = load_config(&config_path(args.config.as_deref(), home.as_deref()))?;
    let state_dir = state_dir(
        args.state_dir.as_deref(),
        home.as_deref(),
        Some(&config.state_dir),
    )?;
    let store = dsf::store::Store::open(&state_dir.join("dsf.sqlite"))?;
    Ok((config, store))
}

fn init_labels(repo: &RepoSlug, args: &GitHubArgs) -> Result<()> {
    let home = std::env::var_os("HOME").map(PathBuf::from);
    let (config, github) = connect_github(args, home.as_deref())?;
    let repo = &config
        .repo(repo)
        .with_context(|| format!("{repo} is not one of the configured repos"))?
        .slug;
    let runtime = tokio::runtime::Runtime::new()?;
    let done = runtime.block_on(dsf::factory::init_labels(&github, repo))?;
    println!("{repo}:");
    for line in done {
        println!("  {line}");
    }
    Ok(())
}

/// The config, and a GitHub client acting as the config's app.
fn connect_github(
    args: &GitHubArgs,
    home: Option<&Path>,
) -> Result<(Config, Arc<dsf::github::GitHub>)> {
    let config_file = config_path(args.config.as_deref(), home);
    let config = load_config(&config_file)?;
    let app_id = config
        .github_app_id
        .with_context(|| format!("{}: set github.app_id", config_file.display()))?;
    let key_file = match &args.github_key {
        Some(file) => file.clone(),
        None => credential("github-app.pem", home)?,
    };
    let key = dsf::github::app::AppKey::load(app_id, &key_file)?;
    let github = dsf::github::GitHub::new(key, &args.github_api)?;
    Ok((config, Arc::new(github)))
}

fn config_path(explicit: Option<&Path>, home: Option<&Path>) -> PathBuf {
    if let Some(file) = explicit {
        return file.to_owned();
    }
    if let Some(file) = std::env::var_os("DSF_CONFIG") {
        return PathBuf::from(file);
    }
    home.map(|home| home.join(".config/dsf/config.toml"))
        .filter(|file| file.is_file())
        .unwrap_or_else(|| PathBuf::from("/etc/dsf/config.toml"))
}

/// A secret file: from systemd's credentials directory when running as a
/// service, otherwise from ~/.config/dsf.
fn credential(name: &str, home: Option<&Path>) -> Result<PathBuf> {
    if let Some(dir) = std::env::var_os("CREDENTIALS_DIRECTORY") {
        return Ok(PathBuf::from(dir).join(name));
    }
    match home {
        Some(home) => Ok(home.join(".config/dsf").join(name)),
        None => bail!("set HOME, or pass the {name} file explicitly"),
    }
}

fn state_dir(
    explicit: Option<&Path>,
    home: Option<&Path>,
    configured: Option<&Path>,
) -> Result<PathBuf> {
    let dir = match (explicit, configured, home) {
        (Some(dir), _, _) | (None, Some(dir), _) => dir.to_owned(),
        (None, None, Some(home)) => home.join(".local/state/dsf"),
        (None, None, None) => bail!("set --state-dir or HOME"),
    };
    std::fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
    Ok(dir.canonicalize()?)
}

fn claude_token(args: &WorkerArgs, home: Option<&Path>) -> Result<secrecy::SecretString> {
    match (&args.token_file, args.egress) {
        (Some(file), _) => read_token(file),
        (None, EgressArg::Proxy) => read_token(&credential("claude-oauth-token", home)?),
        // Offline runs use fake agents, which need no token.
        (None, EgressArg::None) => Ok(secrecy::SecretString::from("offline-no-token")),
    }
}

/// Sets up the agents' network and the proxy sidecar, unless offline.
async fn egress(
    sandbox: &dsf::sandbox::Sandbox,
    args: &WorkerArgs,
    allow: &[String],
) -> Result<dsf::engine::Egress> {
    const NETWORK: &str = "dsf-agents";
    const PROXY: &str = "dsf-proxy";
    if args.egress == EgressArg::None {
        return Ok(dsf::engine::Egress::None);
    }
    sandbox.ensure_internal_network(NETWORK).await?;
    sandbox
        .ensure_proxy(&dsf::sandbox::ProxySidecar {
            name: PROXY,
            image: &args.dsf_image,
            internal_network: NETWORK,
            allow,
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
        comments: Vec::new(),
        require_approval: args.require_approval,
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
