//! Hardened, throwaway containers for agent and verify stages.
//!
//! Every container gets the same settings, whatever the caller asks for:
//! - a non-root user, all capabilities dropped and `no-new-privileges`;
//! - a read-only root filesystem with tmpfs `/tmp` and `HOME`;
//! - PID, memory and CPU limits, and an init process to reap zombies;
//! - only the environment and mounts in its [`Spec`], and never the
//!   Docker socket;
//! - `dsf.run` and `dsf.attempt` labels, so leftovers can be found and
//!   removed after a crash.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use bollard::Docker;
use bollard::errors::Error as DockerError;
use bollard::models::{
    ContainerCreateBody, HostConfig, Mount, MountType, NetworkConnectRequest, NetworkCreateRequest,
    RestartPolicy, RestartPolicyNameEnum,
};
use bollard::query_parameters::{
    CreateContainerOptionsBuilder, KillContainerOptionsBuilder, ListContainersOptionsBuilder,
    LogsOptionsBuilder, RemoveContainerOptionsBuilder,
};
use futures_util::StreamExt;
use secrecy::{ExposeSecret, SecretString};
use tokio::io::AsyncWriteExt;

pub const RUN_LABEL: &str = "dsf.run";
pub const ATTEMPT_LABEL: &str = "dsf.attempt";
/// Long-lived infrastructure (the proxy, the network) carries this instead
/// of [`RUN_LABEL`], so cleaning up after runs never touches it.
pub const ROLE_LABEL: &str = "dsf.role";
const PROXY_CONFIG_LABEL: &str = "dsf.proxy-config";
/// The port the proxy sidecar listens on inside the agents' network.
pub const PROXY_PORT: u16 = 3128;
/// HOME inside every container: a tmpfs, so nothing persists between runs.
pub const HOME: &str = "/home/agent";

const KILL_GRACE: Duration = Duration::from_secs(10);

/// A value for a container's environment. Secrets stay wrapped until the
/// request to Docker is built.
#[derive(Clone)]
pub enum EnvValue {
    Plain(String),
    Secret(SecretString),
}

#[derive(Debug, Clone)]
pub struct Bind {
    pub source: PathBuf,
    pub target: String,
    pub read_only: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Network {
    /// No network at all: verify's default.
    None,
    /// A named Docker network, normally the internal agents' network
    /// whose only way out is the egress proxy.
    Named(String),
}

#[derive(Debug, Clone, Copy)]
pub struct Limits {
    pub memory_bytes: i64,
    /// CPU in billionths of a core: 2 cores is `2_000_000_000`.
    pub nano_cpus: i64,
    pub pids: i64,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            memory_bytes: 3 * 1024 * 1024 * 1024,
            nano_cpus: 2_000_000_000,
            pids: 512,
        }
    }
}

/// One container to run to completion.
pub struct Spec {
    pub name: String,
    pub image: String,
    pub cmd: Vec<String>,
    /// The container's entire environment. Nothing is inherited.
    pub env: Vec<(String, EnvValue)>,
    pub binds: Vec<Bind>,
    pub network: Network,
    pub labels: Vec<(String, String)>,
    /// `uid:gid`. Matching the host user keeps file ownership in bind
    /// mounts consistent.
    pub user: String,
    pub workdir: String,
    pub limits: Limits,
    pub timeout: Duration,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Finished {
    pub container_id: String,
    /// `None` if the container was killed on timeout.
    pub exit_code: Option<i64>,
    pub timed_out: bool,
}

#[derive(Clone)]
pub struct Sandbox {
    docker: Docker,
}

impl Sandbox {
    pub fn connect() -> Result<Self> {
        let docker = Docker::connect_with_local_defaults().context("connecting to Docker")?;
        Ok(Self { docker })
    }

    pub async fn ping(&self) -> Result<()> {
        self.docker
            .ping()
            .await
            .context("Docker is not responding")?;
        Ok(())
    }

    /// Runs `spec` to completion, writing its stdout and stderr to files.
    /// `on_created` gets the container id before it starts, so it can be
    /// recorded for cleanup if dsf dies mid-stage. The container is always
    /// removed afterwards.
    pub async fn run(
        &self,
        spec: &Spec,
        stdout: &Path,
        stderr: &Path,
        on_created: impl FnOnce(&str) -> Result<()>,
    ) -> Result<Finished> {
        check(spec)?;
        let options = CreateContainerOptionsBuilder::new()
            .name(&spec.name)
            .build();
        let created = self
            .docker
            .create_container(Some(options), container_body(spec))
            .await
            .with_context(|| format!("creating container {}", spec.name))?;
        let id = created.id;
        let outcome = async {
            on_created(&id)?;
            self.docker
                .start_container(&id, None)
                .await
                .with_context(|| format!("starting container {}", spec.name))?;
            let logs = tokio::spawn(copy_logs(
                self.docker.clone(),
                id.clone(),
                stdout.to_owned(),
                stderr.to_owned(),
            ));
            let finished =
                if let Ok(code) = tokio::time::timeout(spec.timeout, self.wait(&id)).await {
                    Finished {
                        container_id: id.clone(),
                        exit_code: Some(code?),
                        timed_out: false,
                    }
                } else {
                    self.kill(&id).await;
                    Finished {
                        container_id: id.clone(),
                        exit_code: None,
                        timed_out: true,
                    }
                };
            // The log stream ends when the container stops.
            tokio::time::timeout(KILL_GRACE, logs)
                .await
                .context("log stream did not finish")???;
            Ok(finished)
        }
        .await;
        self.remove(&id).await;
        outcome
    }

    async fn wait(&self, id: &str) -> Result<i64> {
        let mut stream = self.docker.wait_container(id, None);
        match stream.next().await {
            Some(Ok(response)) => Ok(response.status_code),
            // bollard reports a non-zero exit as an error carrying the code.
            Some(Err(DockerError::DockerContainerWaitError { code, .. })) => Ok(code),
            Some(Err(error)) => Err(error).context("waiting for container"),
            None => bail!("container wait ended without a status"),
        }
    }

    async fn kill(&self, id: &str) {
        let options = KillContainerOptionsBuilder::new().signal("SIGKILL").build();
        let _ = self.docker.kill_container(id, Some(options)).await;
        let _ = tokio::time::timeout(KILL_GRACE, self.wait(id)).await;
    }

    async fn remove(&self, id: &str) {
        let options = RemoveContainerOptionsBuilder::new()
            .force(true)
            .v(true)
            .build();
        let _ = self.docker.remove_container(id, Some(options)).await;
    }

    /// Removes every container carrying `label` (e.g. `dsf.run=12`, or just
    /// `dsf.run` for all of them), returning how many there were.
    pub async fn remove_labelled(&self, label: &str) -> Result<usize> {
        let filters = HashMap::from([("label", vec![label])]);
        let options = ListContainersOptionsBuilder::new()
            .all(true)
            .filters(&filters)
            .build();
        let containers = self.docker.list_containers(Some(options)).await?;
        for container in &containers {
            if let Some(id) = &container.id {
                self.remove(id).await;
            }
        }
        Ok(containers.len())
    }

    /// Creates an internal network (no route out) if it doesn't exist.
    pub async fn ensure_internal_network(&self, name: &str) -> Result<()> {
        if self.docker.inspect_network(name, None).await.is_ok() {
            return Ok(());
        }
        self.docker
            .create_network(NetworkCreateRequest {
                name: name.to_owned(),
                internal: Some(true),
                labels: Some(HashMap::from([(
                    ROLE_LABEL.to_owned(),
                    "network".to_owned(),
                )])),
                ..NetworkCreateRequest::default()
            })
            .await
            .with_context(|| format!("creating network {name}"))?;
        Ok(())
    }
}

/// The egress proxy sidecar: `dsf proxy` from the dsf image, on Docker's
/// default bridge (its way out) and on the agents' internal network.
pub struct ProxySidecar<'a> {
    pub name: &'a str,
    pub image: &'a str,
    pub internal_network: &'a str,
    pub allow: &'a [String],
}

impl Sandbox {
    /// Starts the proxy sidecar, or leaves it alone if it is already running
    /// with the same image and allowlist. A changed allowlist replaces it.
    pub async fn ensure_proxy(&self, proxy: &ProxySidecar<'_>) -> Result<()> {
        let config = format!("{}|{}", proxy.image, proxy.allow.join(","));
        if let Ok(existing) = self.docker.inspect_container(proxy.name, None).await {
            let same = existing
                .config
                .and_then(|c| c.labels)
                .is_some_and(|labels| labels.get(PROXY_CONFIG_LABEL) == Some(&config));
            let running = existing.state.and_then(|s| s.running) == Some(true);
            if same && running {
                return Ok(());
            }
            self.remove(proxy.name).await;
        }
        let mut cmd = vec![
            "proxy".to_owned(),
            "--listen".to_owned(),
            format!("0.0.0.0:{PROXY_PORT}"),
        ];
        for host in proxy.allow {
            cmd.push("--allow".to_owned());
            cmd.push(host.clone());
        }
        let body = ContainerCreateBody {
            image: Some(proxy.image.to_owned()),
            cmd: Some(cmd),
            env: Some(Vec::new()),
            user: Some("65534:65534".to_owned()),
            labels: Some(HashMap::from([
                (ROLE_LABEL.to_owned(), "proxy".to_owned()),
                (PROXY_CONFIG_LABEL.to_owned(), config),
            ])),
            host_config: Some(HostConfig {
                readonly_rootfs: Some(true),
                cap_drop: Some(vec!["ALL".to_owned()]),
                security_opt: Some(vec!["no-new-privileges:true".to_owned()]),
                privileged: Some(false),
                network_mode: Some("bridge".to_owned()),
                memory: Some(128 * 1024 * 1024),
                memory_swap: Some(128 * 1024 * 1024),
                pids_limit: Some(256),
                init: Some(true),
                restart_policy: Some(RestartPolicy {
                    name: Some(RestartPolicyNameEnum::UNLESS_STOPPED),
                    maximum_retry_count: None,
                }),
                ..HostConfig::default()
            }),
            ..ContainerCreateBody::default()
        };
        let options = CreateContainerOptionsBuilder::new()
            .name(proxy.name)
            .build();
        self.docker
            .create_container(Some(options), body)
            .await
            .context("creating the proxy sidecar")?;
        self.docker
            .connect_network(
                proxy.internal_network,
                NetworkConnectRequest {
                    container: proxy.name.to_owned(),
                    endpoint_config: None,
                },
            )
            .await
            .context("attaching the proxy to the agents' network")?;
        self.docker
            .start_container(proxy.name, None)
            .await
            .context("starting the proxy sidecar")?;
        Ok(())
    }
}

/// `uid:gid` of the owner of `dir`, which should be a directory dsf
/// created. Containers run as this, so files they write in bind mounts
/// stay owned by the factory user.
pub fn owner_of(dir: &Path) -> Result<String> {
    use std::os::unix::fs::MetadataExt;
    let metadata = std::fs::metadata(dir).with_context(|| format!("reading {}", dir.display()))?;
    Ok(format!("{}:{}", metadata.uid(), metadata.gid()))
}

/// Refuses specs that would undo the isolation: the Docker socket, which
/// is root on the host, and networks that share the host's or another
/// container's stack.
fn check(spec: &Spec) -> Result<()> {
    for bind in &spec.binds {
        let source = bind.source.to_string_lossy();
        if source.ends_with("docker.sock") || bind.target.ends_with("docker.sock") {
            bail!("refusing to mount the Docker socket into {}", spec.name);
        }
    }
    if let Network::Named(name) = &spec.network
        && (name == "host" || name.starts_with("container:"))
    {
        bail!("refusing network mode `{name}` for {}", spec.name);
    }
    Ok(())
}

/// The create request. Everything security-relevant is fixed here rather
/// than taken from the caller.
fn container_body(spec: &Spec) -> ContainerCreateBody {
    let (uid, gid) = spec
        .user
        .split_once(':')
        .unwrap_or((&spec.user, &spec.user));
    let tmpfs = HashMap::from([
        ("/tmp".to_owned(), "rw,nosuid,nodev,size=512m".to_owned()),
        (
            HOME.to_owned(),
            format!("rw,nosuid,nodev,size=512m,uid={uid},gid={gid}"),
        ),
    ]);
    let mounts = spec
        .binds
        .iter()
        .map(|bind| Mount {
            typ: Some(MountType::BIND),
            source: Some(bind.source.to_string_lossy().into_owned()),
            target: Some(bind.target.clone()),
            read_only: Some(bind.read_only),
            ..Mount::default()
        })
        .collect();
    let network_mode = match &spec.network {
        Network::None => "none".to_owned(),
        Network::Named(name) => name.clone(),
    };
    let env = spec
        .env
        .iter()
        .map(|(key, value)| match value {
            EnvValue::Plain(value) => format!("{key}={value}"),
            EnvValue::Secret(secret) => format!("{key}={}", secret.expose_secret()),
        })
        .collect();
    ContainerCreateBody {
        image: Some(spec.image.clone()),
        cmd: Some(spec.cmd.clone()),
        entrypoint: Some(Vec::new()),
        env: Some(env),
        user: Some(spec.user.clone()),
        working_dir: Some(spec.workdir.clone()),
        labels: Some(spec.labels.iter().cloned().collect()),
        network_disabled: Some(spec.network == Network::None),
        host_config: Some(HostConfig {
            readonly_rootfs: Some(true),
            cap_drop: Some(vec!["ALL".to_owned()]),
            security_opt: Some(vec!["no-new-privileges:true".to_owned()]),
            privileged: Some(false),
            tmpfs: Some(tmpfs),
            mounts: Some(mounts),
            network_mode: Some(network_mode),
            memory: Some(spec.limits.memory_bytes),
            memory_swap: Some(spec.limits.memory_bytes),
            nano_cpus: Some(spec.limits.nano_cpus),
            pids_limit: Some(spec.limits.pids),
            ipc_mode: Some("private".to_owned()),
            init: Some(true),
            auto_remove: Some(false),
            ..HostConfig::default()
        }),
        ..ContainerCreateBody::default()
    }
}

async fn copy_logs(docker: Docker, id: String, stdout: PathBuf, stderr: PathBuf) -> Result<()> {
    let mut out = tokio::fs::File::create(&stdout).await?;
    let mut err = tokio::fs::File::create(&stderr).await?;
    let options = LogsOptionsBuilder::new()
        .follow(true)
        .stdout(true)
        .stderr(true)
        .build();
    let mut stream = docker.logs(&id, Some(options));
    while let Some(chunk) = stream.next().await {
        match chunk? {
            bollard::container::LogOutput::StdErr { message } => err.write_all(&message).await?,
            bollard::container::LogOutput::StdOut { message }
            | bollard::container::LogOutput::Console { message } => {
                out.write_all(&message).await?;
            }
            bollard::container::LogOutput::StdIn { .. } => {}
        }
    }
    out.flush().await?;
    err.flush().await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec() -> Spec {
        Spec {
            name: "dsf-1-2".into(),
            image: "worker:test".into(),
            cmd: vec!["true".into()],
            env: vec![
                ("HOME".into(), EnvValue::Plain(HOME.into())),
                (
                    "CLAUDE_CODE_OAUTH_TOKEN".into(),
                    EnvValue::Secret(SecretString::from("sk-ant-test")),
                ),
            ],
            binds: vec![Bind {
                source: "/runs/1/repo.git".into(),
                target: "/work/.git".into(),
                read_only: true,
            }],
            network: Network::None,
            labels: vec![
                (RUN_LABEL.into(), "1".into()),
                (ATTEMPT_LABEL.into(), "2".into()),
            ],
            user: "1000:1000".into(),
            workdir: "/work".into(),
            limits: Limits::default(),
            timeout: Duration::from_secs(60),
        }
    }

    #[test]
    fn every_container_is_hardened() {
        let body = container_body(&spec());
        let host = body.host_config.unwrap();
        assert_eq!(host.readonly_rootfs, Some(true));
        assert_eq!(host.cap_drop, Some(vec!["ALL".to_owned()]));
        assert_eq!(
            host.security_opt,
            Some(vec!["no-new-privileges:true".to_owned()])
        );
        assert_eq!(host.privileged, Some(false));
        assert_eq!(host.network_mode.as_deref(), Some("none"));
        assert_eq!(
            host.memory, host.memory_swap,
            "no swap beyond the memory limit"
        );
        assert!(host.tmpfs.unwrap()[HOME].contains("uid=1000,gid=1000"));
        assert_eq!(body.network_disabled, Some(true));
        assert_eq!(
            body.entrypoint,
            Some(Vec::new()),
            "the image can't wrap the command"
        );
        assert_eq!(body.user.as_deref(), Some("1000:1000"));
    }

    #[test]
    fn the_environment_is_exactly_the_spec() {
        let env = container_body(&spec()).env.unwrap();
        assert_eq!(
            env,
            ["HOME=/home/agent", "CLAUDE_CODE_OAUTH_TOKEN=sk-ant-test"]
        );
    }

    #[test]
    fn mounts_keep_their_read_only_flag() {
        let mounts = container_body(&spec()).host_config.unwrap().mounts.unwrap();
        assert_eq!(mounts.len(), 1);
        assert_eq!(mounts[0].read_only, Some(true));
        assert_eq!(mounts[0].target.as_deref(), Some("/work/.git"));
    }

    #[test]
    fn refuses_the_docker_socket_and_shared_network_stacks() {
        assert!(check(&spec()).is_ok());
        let mut socket = spec();
        socket.binds.push(Bind {
            source: "/var/run/docker.sock".into(),
            target: "/run/x".into(),
            read_only: true,
        });
        assert!(check(&socket).is_err());
        for network in ["host", "container:proxy"] {
            let mut shared = spec();
            shared.network = Network::Named(network.into());
            assert!(check(&shared).is_err(), "{network}");
        }
    }
}
