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
    if args.first() == Some(&"update") && args.get(1) == Some(&"codex") {
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
    let out = cli(t.path(), &["update", "codex"]).output().unwrap();
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
        let out = cli(t.path(), &["update", "codex"])
            .env(flag, "1")
            .output()
            .unwrap();
        assert!(!out.status.success());
        assert!(!t.path().join("restarted").exists());
        assert!(!t.path().join(".local/bin/codex").exists());
    }
    let t = fixture();
    assert!(cli(t.path(), &["update", "codex", "--no-restart"])
        .status()
        .unwrap()
        .success());
    assert!(!t.path().join("restarted").exists());
}
#[test]
fn update_dry_run_does_not_install() {
    let t = fixture();
    assert!(cli(t.path(), &["update", "codex", "--dry-run"])
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

#[test]
fn limits_parallel_refresh_and_failure_are_isolated() {
    let t = tempfile::tempdir().unwrap();
    let home = t.path();
    fs::create_dir_all(home.join(".codex/accounts")).unwrap();
    for n in ["alpha", "beta", "slow"] {
        let path = home.join(format!(".codex/accounts/{n}.json"));
        fs::write(&path, format!(r#"{{"name":"{n}","token":"old"}}"#)).unwrap();
        fs::set_permissions(path, fs::Permissions::from_mode(0o600)).unwrap();
    }
    std::os::unix::fs::symlink("accounts/alpha.json", home.join(".codex/auth.json")).unwrap();
    let fake = home.join("codex");
    executable(
        &fake,
        r#"#!/usr/bin/env python3
import json, os, sys, time
p=os.path.join(os.environ['CODEX_HOME'],'auth.json')
a=json.load(open(p))
for line in sys.stdin:
 v=json.loads(line)
 if 'id' not in v: continue
 method=v['method']
 if method=='account/rateLimits/read':
  if a['name']=='slow': time.sleep(5)
  else: time.sleep(.35)
  a['token']='new'
  with open(p,'w') as f: json.dump(a,f)
  result={'rateLimits':{'secondary':{'windowDurationMins':10080,'usedPercent':42,'resetsAt':1800000000}}}
 else: result={}
 print(json.dumps({'id':v['id'],'result':result}),flush=True)
"#,
    );
    let start = std::time::Instant::now();
    let out = Command::new(env!("CARGO_BIN_EXE_codex-appserver-ctl"))
        .args(["limits", "--timeout", "1"])
        .env("HOME", home)
        .env("CODEX_APPSERVER_HOST_CODEX", &fake)
        .output()
        .unwrap();
    assert!(!out.status.success());
    assert!(
        start.elapsed().as_secs_f64() < 2.5,
        "checks must run concurrently"
    );
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(
        text.contains("42%") && text.contains("58%") && text.contains("timed out"),
        "{text}"
    );
    assert!(!text.contains("\x1b"));
    let alpha: serde_json::Value =
        serde_json::from_slice(&fs::read(home.join(".codex/accounts/alpha.json")).unwrap())
            .unwrap();
    assert_eq!(alpha["token"], "new");
    let cache_path = home.join(".codex/appserver-ctl-limits.json");
    let cached = fs::read(&cache_path).unwrap();
    let saved: serde_json::Value = serde_json::from_slice(&cached).unwrap();
    assert_eq!(saved["alpha"]["windows"][0]["used"].as_f64(), Some(42.));
    assert!(saved["alpha"]["updated_at"].as_i64().unwrap() > 0);
    assert!(!String::from_utf8_lossy(&cached).contains("token"));
    assert_eq!(
        fs::metadata(&cache_path).unwrap().permissions().mode() & 0o777,
        0o600
    );
    executable(&fake, "#!/bin/sh\nexit 1\n");
    let stale = Command::new(env!("CARGO_BIN_EXE_codex-appserver-ctl"))
        .args(["limits", "--timeout", "1"])
        .env("HOME", home)
        .env("CODEX_APPSERVER_HOST_CODEX", &fake)
        .output()
        .unwrap();
    let text = String::from_utf8_lossy(&stale.stdout);
    assert!(!stale.status.success());
    assert!(
        text.contains("42%") && text.contains("STALE") && text.contains("LAST UPDATED"),
        "{text}"
    );
    let after: serde_json::Value = serde_json::from_slice(&fs::read(cache_path).unwrap()).unwrap();
    assert_eq!(after["alpha"]["updated_at"], saved["alpha"]["updated_at"]);

    assert_eq!(
        fs::read_link(home.join(".codex/auth.json")).unwrap(),
        Path::new("accounts/alpha.json")
    );
}

#[test]
fn self_update_uses_release_installer_and_current_prefix() {
    let t = fixture();
    let bin = t.path().join("custom/bin");
    fs::create_dir_all(&bin).unwrap();
    let ctl = bin.join("codex-appserver-ctl");
    fs::copy(env!("CARGO_BIN_EXE_codex-appserver-ctl"), &ctl).unwrap();
    executable(
        &t.path().join("installer"),
        r#"#!/bin/sh
[ "$1" = --prefix ] || exit 3
printf '#!/bin/sh\necho "codex-appserver-ctl NEW"\n' > "$2/bin/codex-appserver-ctl.tmp"
chmod +x "$2/bin/codex-appserver-ctl.tmp"
mv "$2/bin/codex-appserver-ctl.tmp" "$2/bin/codex-appserver-ctl"
"#,
    );
    let out = Command::new(&ctl)
        .arg("update")
        .env("HOME", t.path())
        .env(
            "PATH",
            format!("{}:/usr/bin:/bin", t.path().join("bin").display()),
        )
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(String::from_utf8_lossy(&out.stdout).contains("NEW"));
    assert!(!t.path().join("restarted").exists());
}
