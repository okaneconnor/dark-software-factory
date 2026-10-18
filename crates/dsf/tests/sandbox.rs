//! Containers checked from the inside. These need Docker, so they only run
//! with `DSF_DOCKER_TESTS=1`.

// Helpers outside #[test] functions aren't covered by clippy's
// allow-unwrap-in-tests, and a failed precondition should panic here.
#![allow(clippy::unwrap_used)]

use std::path::Path;
use std::time::Duration;

use dsf::sandbox::{
    ATTEMPT_LABEL, Bind, EnvValue, Finished, HOME, Limits, Network, RUN_LABEL, Sandbox, Spec,
    owner_of,
};

const IMAGE: &str = "alpine:3.20";

fn enabled() -> bool {
    if std::env::var_os("DSF_DOCKER_TESTS").is_some_and(|v| v == "1") {
        let present = std::process::Command::new("docker")
            .args(["image", "inspect", IMAGE])
            .output()
            .unwrap();
        if !present.status.success() {
            let pulled = std::process::Command::new("docker")
                .args(["pull", "--quiet", IMAGE])
                .status()
                .unwrap();
            assert!(pulled.success(), "could not pull {IMAGE}");
        }
        true
    } else {
        eprintln!("skipped: set DSF_DOCKER_TESTS=1 to run Docker tests");
        false
    }
}

fn spec(dir: &Path, name: &str, script: &str) -> Spec {
    Spec {
        name: format!("dsf-test-{name}-{}", std::process::id()),
        image: IMAGE.into(),
        cmd: vec!["sh".into(), "-c".into(), script.into()],
        env: vec![
            ("HOME".into(), EnvValue::Plain(HOME.into())),
            (
                "CLAUDE_CODE_OAUTH_TOKEN".into(),
                EnvValue::Secret("sk-ant-not-a-real-token".to_owned().into()),
            ),
        ],
        binds: Vec::new(),
        network: Network::None,
        labels: vec![
            (RUN_LABEL.into(), "test".into()),
            (ATTEMPT_LABEL.into(), name.into()),
        ],
        user: owner_of(dir).unwrap(),
        workdir: "/".into(),
        limits: Limits {
            memory_bytes: 256 * 1024 * 1024,
            nano_cpus: 500_000_000,
            pids: 64,
        },
        timeout: Duration::from_secs(60),
    }
}

async fn run(sandbox: &Sandbox, dir: &Path, spec: &Spec) -> (Finished, String, String) {
    let (out, err) = (
        dir.join(format!("{}.out", spec.name)),
        dir.join(format!("{}.err", spec.name)),
    );
    let finished = sandbox.run(spec, &out, &err, |_| Ok(())).await.unwrap();
    (
        finished,
        std::fs::read_to_string(out).unwrap(),
        std::fs::read_to_string(err).unwrap(),
    )
}

#[tokio::test]
async fn the_hardening_holds_from_inside() {
    if !enabled() {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let sandbox = Sandbox::connect().unwrap();
    let script = r#"
        echo "uid=$(id -u)"
        touch /rootfs-test 2>/dev/null && echo "rootfs=writable" || echo "rootfs=read-only"
        echo hi > "$HOME/x" && echo "home=writable"
        grep -E '^(CapEff|CapPrm|CapBnd|NoNewPrivs):' /proc/self/status
        test -e /var/run/docker.sock && echo "socket=present" || echo "socket=absent"
        test -e /sys/class/net/eth0 && echo "eth0=present" || echo "eth0=absent"
        echo "routes=$(tail -n +2 /proc/net/route | wc -l)"
        # PID 1 is the init process, which has exactly the container's environment.
        tr '\0' '\n' < /proc/1/environ | sort | sed 's/^/env:/' 
    "#;
    let (finished, out, err) =
        run(&sandbox, dir.path(), &spec(dir.path(), "hardening", script)).await;
    assert_eq!(finished.exit_code, Some(0), "{err}");
    let uid = owner_of(dir.path()).unwrap();
    assert!(
        out.contains(&format!("uid={}", uid.split(':').next().unwrap())),
        "{out}"
    );
    assert!(out.contains("rootfs=read-only"), "{out}");
    assert!(out.contains("home=writable"), "{out}");
    assert!(out.contains("CapEff:\t0000000000000000"), "{out}");
    assert!(out.contains("CapBnd:\t0000000000000000"), "{out}");
    assert!(out.contains("NoNewPrivs:\t1"), "{out}");
    assert!(out.contains("socket=absent"), "{out}");
    assert!(out.contains("eth0=absent"), "{out}");
    assert!(
        out.contains("routes=0"),
        "no routes without a network: {out}"
    );
    // Exactly the spec's environment, plus the PATH and HOSTNAME Docker sets.
    let names: Vec<&str> = out
        .lines()
        .filter_map(|line| line.strip_prefix("env:"))
        .map(|line| line.split('=').next().unwrap())
        .collect();
    assert_eq!(
        names,
        ["CLAUDE_CODE_OAUTH_TOKEN", "HOME", "HOSTNAME", "PATH"],
        "{out}"
    );
}

#[tokio::test]
async fn binds_keep_their_read_only_flag() {
    if !enabled() {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let (rw, ro) = (dir.path().join("rw"), dir.path().join("ro"));
    std::fs::create_dir(&rw).unwrap();
    std::fs::create_dir(&ro).unwrap();
    let sandbox = Sandbox::connect().unwrap();
    let mut spec = spec(
        dir.path(),
        "binds",
        "echo a > /rw/a && (echo b > /ro/b 2>/dev/null || echo ro-refused)",
    );
    spec.binds = vec![
        Bind {
            source: rw.clone(),
            target: "/rw".into(),
            read_only: false,
        },
        Bind {
            source: ro.clone(),
            target: "/ro".into(),
            read_only: true,
        },
    ];
    let (finished, out, _) = run(&sandbox, dir.path(), &spec).await;
    assert_eq!(finished.exit_code, Some(0));
    assert!(out.contains("ro-refused"), "{out}");
    assert_eq!(std::fs::read_to_string(rw.join("a")).unwrap(), "a\n");
    assert!(!ro.join("b").exists());
}

#[tokio::test]
async fn exit_codes_and_timeouts_are_reported_and_containers_removed() {
    if !enabled() {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let sandbox = Sandbox::connect().unwrap();
    let (failed, _, err) = run(
        &sandbox,
        dir.path(),
        &spec(dir.path(), "exit", "echo oops >&2; exit 3"),
    )
    .await;
    assert_eq!(failed.exit_code, Some(3));
    assert_eq!(err, "oops\n");

    let mut slow = spec(dir.path(), "timeout", "sleep 30");
    slow.timeout = Duration::from_secs(2);
    let started = std::time::Instant::now();
    let (finished, _, _) = run(&sandbox, dir.path(), &slow).await;
    assert!(finished.timed_out);
    assert_eq!(finished.exit_code, None);
    assert!(started.elapsed() < Duration::from_secs(20));

    let label = format!("{ATTEMPT_LABEL}=timeout");
    assert_eq!(
        sandbox.remove_labelled(&label).await.unwrap(),
        0,
        "run() should have removed it"
    );
}

#[tokio::test]
async fn containers_left_by_a_crash_are_found_by_label_and_removed() {
    if !enabled() {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let sandbox = Sandbox::connect().unwrap();
    let mut orphan = spec(dir.path(), "orphan", "sleep 60");
    orphan.labels = vec![(RUN_LABEL.into(), format!("crash-{}", std::process::id()))];
    let (out, err) = (dir.path().join("o.out"), dir.path().join("o.err"));
    let (created, created_rx) = tokio::sync::oneshot::channel();
    let task = {
        let sandbox = sandbox.clone();
        tokio::spawn(async move {
            sandbox
                .run(&orphan, &out, &err, |id| {
                    let _ = created.send(id.to_owned());
                    Ok(())
                })
                .await
        })
    };
    created_rx.await.unwrap();
    tokio::time::sleep(Duration::from_millis(500)).await;
    // dsf dies mid-stage: the task never gets to remove its container.
    task.abort();
    let label = format!("{RUN_LABEL}=crash-{}", std::process::id());
    assert_eq!(sandbox.remove_labelled(&label).await.unwrap(), 1);
    assert_eq!(sandbox.remove_labelled(&label).await.unwrap(), 0);
}

#[tokio::test]
async fn an_internal_network_has_no_way_out() {
    if !enabled() {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let sandbox = Sandbox::connect().unwrap();
    let network = format!("dsf-test-internal-{}", std::process::id());
    sandbox.ensure_internal_network(&network).await.unwrap();
    sandbox.ensure_internal_network(&network).await.unwrap();
    let mut spec = spec(
        dir.path(),
        "internal",
        "wget -q -T 3 -O /dev/null http://1.1.1.1 && echo direct=reachable || echo direct=blocked; \
         nslookup example.com >/dev/null 2>&1 && echo dns=works || echo dns=none",
    );
    spec.network = Network::Named(network.clone());
    let (_, out, _) = run(&sandbox, dir.path(), &spec).await;
    std::process::Command::new("docker")
        .args(["network", "rm", &network])
        .output()
        .unwrap();
    assert!(out.contains("direct=blocked"), "{out}");
    assert!(out.contains("dns=none"), "{out}");
}

/// Needs the dsf image (`docker build -f docker/dsf.Dockerfile -t dsf:dev .`)
/// and internet access from the proxy.
#[tokio::test]
async fn the_proxy_sidecar_is_the_only_way_out() {
    const DSF_IMAGE: &str = "dsf:dev";
    if !enabled() {
        return;
    }
    let image_present = std::process::Command::new("docker")
        .args(["image", "inspect", DSF_IMAGE])
        .output()
        .unwrap();
    if !image_present.status.success() {
        eprintln!("skipped: build {DSF_IMAGE} first");
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let sandbox = Sandbox::connect().unwrap();
    let pid = std::process::id();
    let network = format!("dsf-test-agents-{pid}");
    let proxy_name = format!("dsf-test-proxy-{pid}");
    let container_id = |name: &str| {
        let out = std::process::Command::new("docker")
            .args(["inspect", "--format", "{{.Id}}", name])
            .output()
            .unwrap();
        String::from_utf8(out.stdout).unwrap()
    };
    sandbox.ensure_internal_network(&network).await.unwrap();
    let allow = vec!["api.anthropic.com".to_owned()];
    let proxy = dsf::sandbox::ProxySidecar {
        name: &proxy_name,
        image: DSF_IMAGE,
        internal_network: &network,
        allow: &allow,
    };
    sandbox.ensure_proxy(&proxy).await.unwrap();
    let first = container_id(&proxy_name);
    sandbox.ensure_proxy(&proxy).await.unwrap();
    assert_eq!(
        container_id(&proxy_name),
        first,
        "an unchanged proxy is left running"
    );

    let probe = format!(
        "for target in example.com:443 api.anthropic.com:22 api.anthropic.com:443; do \
           printf 'CONNECT %s HTTP/1.1\\r\\n\\r\\n' $target | nc -w 5 {proxy_name} 3128 | head -1 | sed \"s|^|$target |\"; \
         done"
    );
    let mut client = spec(dir.path(), "via-proxy", &probe);
    client.network = Network::Named(network.clone());
    let (_, out, _) = run(&sandbox, dir.path(), &client).await;

    let changed = vec!["example.com".to_owned()];
    sandbox
        .ensure_proxy(&dsf::sandbox::ProxySidecar {
            allow: &changed,
            ..proxy
        })
        .await
        .unwrap();
    let replaced = container_id(&proxy_name) != first;
    std::process::Command::new("docker")
        .args(["rm", "-f", &proxy_name])
        .output()
        .unwrap();
    std::process::Command::new("docker")
        .args(["network", "rm", &network])
        .output()
        .unwrap();

    assert!(
        out.contains("example.com:443 HTTP/1.1 403 Forbidden"),
        "{out}"
    );
    assert!(
        out.contains("api.anthropic.com:22 HTTP/1.1 403 Forbidden"),
        "{out}"
    );
    assert!(
        out.contains("api.anthropic.com:443 HTTP/1.1 200 Connection Established"),
        "{out}"
    );
    assert!(replaced, "a changed allowlist replaces the proxy");
}
