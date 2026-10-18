//! Helpers shared by the Docker-backed test suites.

// Each test crate uses its own subset.
#![allow(dead_code)]
// A failed precondition should panic here.
#![allow(clippy::unwrap_used)]

use std::path::Path;
use std::sync::OnceLock;

pub mod github;

pub const FAKE_IMAGE: &str = "dsf-fake-agent:test";
pub const TOKEN: &str = "sk-ant-oat01-fake-token-for-tests-only-0123456789";

pub fn enabled() -> bool {
    static BUILT: OnceLock<bool> = OnceLock::new();
    if std::env::var_os("DSF_DOCKER_TESTS").is_none_or(|v| v != "1") {
        eprintln!("skipped: set DSF_DOCKER_TESTS=1 to run Docker tests");
        return false;
    }
    *BUILT.get_or_init(|| {
        let context = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fake-agent");
        let status = std::process::Command::new("docker")
            .args(["build", "--quiet", "--tag", FAKE_IMAGE])
            .arg(context)
            .stdout(std::process::Stdio::null())
            .status()
            .unwrap();
        assert!(status.success(), "building {FAKE_IMAGE} failed");
        true
    })
}

pub fn git(dir: &Path, args: &[&str]) -> String {
    let output = std::process::Command::new("git")
        .current_dir(dir)
        .env_clear()
        .env("PATH", std::env::var_os("PATH").unwrap())
        .env("HOME", "/nonexistent")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .args([
            "-c",
            "user.name=test",
            "-c",
            "user.email=test@example.invalid",
            "-c",
            "init.defaultBranch=main",
        ])
        .args(args)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).unwrap()
}

/// A throwaway RSA key from the system's openssl, so no private key is
/// ever committed. Made once per test binary.
pub fn app_key_pem() -> &'static str {
    static KEY: OnceLock<String> = OnceLock::new();
    KEY.get_or_init(|| {
        let output = std::process::Command::new("openssl")
            .args([
                "genpkey",
                "-algorithm",
                "RSA",
                "-pkeyopt",
                "rsa_keygen_bits:2048",
            ])
            .output()
            .unwrap();
        assert!(output.status.success(), "openssl genpkey failed");
        String::from_utf8(output.stdout).unwrap()
    })
}
