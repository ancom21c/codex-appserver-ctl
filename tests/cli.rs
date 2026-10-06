use std::{fs, os::unix::fs::PermissionsExt, path::Path, process::Command};
fn executable(path: &Path, contents: &str) {
    fs::write(path, contents).unwrap();
    fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
}
fn fixture() -> tempfile::TempDir {
    let root = tempfile::tempdir().unwrap();
    fs::create_dir(root.path().join("bin")).unwrap();
    executable(
        &root.path().join("bin/curl"),
        r#"#!/bin/sh
if [ "$TEST_DOWNLOAD_FAIL" = 1 ]; then exit 2; fi
while [ "$#" -gt 0 ]; do
  if [ "$1" = --output ]; then shift; cp "$HOME/installer" "$1"; exit; fi
  shift
done
exit 3
"#,
    );
    executable(
        &root.path().join("installer"),
        r#"#!/bin/sh
[ "$CODEX_RELEASE" = latest ] || exit 3
[ "$CODEX_NON_INTERACTIVE" = 1 ] || exit 4
if [ "$TEST_INSTALL_FAIL" = 1 ]; then exit 2; fi
mkdir -p "$CODEX_INSTALL_DIR"
cp "$HOME/newcodex" "$CODEX_INSTALL_DIR/codex"
"#,
    );
    executable(
        &root.path().join("newcodex"),
        r#"#!/bin/sh
case "$*" in
 --version) echo 'codex-cli TEST' ;;
 'app-server daemon version') echo '{"status":"running"}' ;;
 'app-server daemon restart') touch "$HOME/restarted" ;;
 *) exit 3 ;;
esac
"#,
    );
    root
}
fn cli(root: &Path, args: &[&str]) -> Command {
    let mut c = Command::new(env!("CARGO_BIN_EXE_codex-appserver-ctl"));
    c.args(args);
    if args.first() == Some(&"update") {
        c.arg("--internal-direct");
    }
    c.env("HOME", root).env(
        "PATH",
        format!("{}:/usr/bin:/bin", root.join("bin").display()),
    );
    c
}
#[test]
fn update_installs_official_script_then_restarts() {
    let t = fixture();
    let out = cli(t.path(), &["update"]).output().unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(t.path().join("restarted").is_file());
    assert!(String::from_utf8_lossy(&out.stdout).contains("codex-cli TEST"));
}
#[test]
fn update_failure_never_restarts_and_no_restart_option_works() {
    for flag in ["TEST_DOWNLOAD_FAIL", "TEST_INSTALL_FAIL"] {
        let t = fixture();
        let out = cli(t.path(), &["update"]).env(flag, "1").output().unwrap();
        assert!(!out.status.success());
        assert!(!t.path().join("restarted").exists());
        assert!(!t.path().join(".local/bin/codex").exists());
    }
    let t = fixture();
    assert!(cli(t.path(), &["update", "--no-restart"])
        .status()
        .unwrap()
        .success());
    assert!(!t.path().join("restarted").exists());
}
#[test]
fn update_dry_run_does_not_install() {
    let t = fixture();
    assert!(cli(t.path(), &["update", "--dry-run"])
        .status()
        .unwrap()
        .success());
    assert!(!t.path().join(".local/bin/codex").exists());
}
#[test]
fn usage_runs_without_codex_or_ccusage() {
    let t = tempfile::tempdir().unwrap();
    let out = cli(t.path(), &["usage", "--json"]).output().unwrap();
    assert!(out.status.success());
    let value: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(value["total"]["total_tokens"], 0);
    assert_eq!(value["files"], 0);
}
