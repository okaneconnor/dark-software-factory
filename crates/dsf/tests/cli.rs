//! Black-box tests of the `dsf` binary.

use assert_cmd::Command;

#[test]
fn version_flag_prints_the_crate_version() {
    Command::cargo_bin("dsf")
        .unwrap()
        .arg("--version")
        .assert()
        .success()
        .stdout(format!("dsf {}\n", env!("CARGO_PKG_VERSION")));
}
