use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine as _};
use serde_json::{json, Value};
use std::{fs, os::unix::fs::PermissionsExt, path::Path, process::Command};
use std::{
    io::{Read, Write},
    os::fd::{AsRawFd, FromRawFd},
    process::{Child, Output, Stdio},
    time::{Duration, Instant},
};

fn auth(user: &str, used: u32) -> Value {
    let claims = json!({"https://api.openai.com/auth": {
        "chatgpt_user_id": user, "chatgpt_account_id": "shared-workspace"
    }});
    json!({"name":user,"token":"old","used":used,"tokens":{
        "id_token":format!("header.{}.signature", URL_SAFE_NO_PAD.encode(claims.to_string())),
        "account_id":"shared-workspace"
    }})
}
fn private_json(path: &Path, value: &Value) {
    fs::write(path, value.to_string()).unwrap();
    fs::set_permissions(path, fs::Permissions::from_mode(0o600)).unwrap();
}
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
        private_json(&path, &auth(n, 42));
    }
    std::os::unix::fs::symlink("accounts/alpha.json", home.join(".codex/auth.json")).unwrap();
    let fake = home.join("codex");
    executable(
        &fake,
        r#"#!/bin/sh
p="$CODEX_HOME/auth.json"
case "$(cat "$p")" in
  *'"name":"alpha"'*) name=alpha ;;
  *'"name":"beta"'*) name=beta ;;
  *) name=slow ;;
esac
while IFS= read -r line; do
 case "$line" in
  *'"method":"initialize"'*) echo '{"id":1,"result":{}}' ;;
  *'"method":"account/read"'*) echo '{"id":2,"result":{}}' ;;
  *'"method":"account/rateLimits/read"'*)
   touch "$HOME/started-$name"
   # Successful requests must overlap with every worker, not rely on interpreter timing.
   while [ ! -f "$HOME/started-alpha" ] || [ ! -f "$HOME/started-beta" ] || [ ! -f "$HOME/started-slow" ]; do sleep .05; done
   if [ "$name" = slow ]; then
    while IFS= read -r more; do :; done
    exit
   fi
   sed 's/"token":"old"/"token":"new"/' "$p" > "$p.tmp"
   chmod 600 "$p.tmp"
   mv "$p.tmp" "$p"
   echo '{"id":3,"result":{"rateLimits":{"secondary":{"windowDurationMins":10080,"usedPercent":42,"resetsAt":1800000000}}}}'
   ;;
 esac
done
"#,
    );
    let start = std::time::Instant::now();
    let out = Command::new(env!("CARGO_BIN_EXE_codex-appserver-ctl"))
        .args(["limits", "--timeout", "4"])
        .env("HOME", home)
        .env("CODEX_APPSERVER_HOST_CODEX", &fake)
        .output()
        .unwrap();
    assert!(!out.status.success());
    assert!(
        start.elapsed().as_secs_f64() < 6.,
        "checks must run concurrently"
    );
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(
        text.contains("42%")
            && text.contains("58%")
            && String::from_utf8_lossy(&out.stderr).contains("timed out"),
        "{text}\n{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(!text.contains("\x1b"));
    let alpha: serde_json::Value =
        serde_json::from_slice(&fs::read(home.join(".codex/accounts/alpha.json")).unwrap())
            .unwrap();
    assert_eq!(alpha["token"], "new");
    let cache_path = home.join(".codex/appserver-ctl-limits.json");
    let cached = fs::read(&cache_path).unwrap();
    let saved: serde_json::Value = serde_json::from_slice(&cached).unwrap();
    assert_eq!(
        saved["profile:alpha"]["windows"][0]["used"].as_f64(),
        Some(42.)
    );
    assert!(saved["profile:alpha"]["updated_at"].as_i64().unwrap() > 0);
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
    assert_eq!(
        after["profile:alpha"]["updated_at"],
        saved["profile:alpha"]["updated_at"]
    );

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

fn limits_fixture() -> tempfile::TempDir {
    let t = tempfile::tempdir().unwrap();
    fs::create_dir_all(t.path().join(".codex/accounts")).unwrap();
    private_json(
        &t.path().join(".codex/accounts/alpha.json"),
        &auth("alpha", 11),
    );
    executable(
        &t.path().join("codex"),
        r#"#!/usr/bin/env python3
import json, os, sys, time
p=os.path.join(os.environ['CODEX_HOME'],'auth.json')
a=json.load(open(p))
mode=os.environ.get('TEST_RPC_MODE','ok')
for line in sys.stdin:
 v=json.loads(line)
 if 'id' not in v: continue
 if v['method']=='account/read' and mode.startswith('rotate'):
  a['token']='new'
  with open(p,'w') as f: json.dump(a,f)
  if mode=='rotate-error':
   print(json.dumps({'id':v['id'],'error':{'code':-1}}),flush=True)
   continue
 if v['method']=='account/rateLimits/read':
  if mode=='fail': sys.exit(1)
  if mode=='replace':
   replacement=json.load(open(os.path.join(os.environ['HOME'],'replacement')))
   with open(os.path.join(os.environ['HOME'],'.codex/accounts/alpha.json'),'w') as f: json.dump(replacement,f)
  if mode=='rotate-hang':
   with open(os.path.join(os.environ['HOME'],'ready'),'w') as f: json.dump({'pid':os.getpid(),'parent':os.getppid(),'auth':p},f)
   time.sleep(30)
  if mode=='delay': time.sleep(.4)
  result={'rateLimits':{'secondary':{'windowDurationMins':10080,'usedPercent':a['used'],'resetsAt':1800000000}}}
 else: result={}
 print(json.dumps({'id':v['id'],'result':result}),flush=True)
"#,
    );
    t
}
fn limits(home: &Path, mode: &str) -> Command {
    let mut command = cli(home, &["limits", "--timeout", "30"]);
    command
        .env("CODEX_APPSERVER_HOST_CODEX", home.join("codex"))
        .env("TEST_RPC_MODE", mode);
    command
}
fn cache(home: &Path) -> Value {
    serde_json::from_slice(&fs::read(home.join(".codex/appserver-ctl-limits.json")).unwrap())
        .unwrap()
}
fn wait_file(path: &Path) {
    let start = Instant::now();
    while !path.exists() {
        assert!(
            start.elapsed() < Duration::from_secs(5),
            "fixture never became ready"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
}
fn finish(mut child: Child) -> Output {
    let start = Instant::now();
    while child.try_wait().unwrap().is_none() {
        if start.elapsed() > Duration::from_secs(5) {
            child.kill().unwrap();
            let _ = child.wait();
            panic!("CLI did not finish within five seconds");
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    child.wait_with_output().unwrap()
}
#[test]
fn cancellation_and_rpc_failure_preserve_rotated_credentials() {
    for blocked_save in [false, true] {
        let t = limits_fixture();
        let home = t.path();
        let child = limits(home, "rotate-hang")
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        wait_file(&home.join("ready"));
        let ready: Value = serde_json::from_slice(&fs::read(home.join("ready")).unwrap()).unwrap();
        let lock = fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(home.join(".codex/appserver-ctl-auth.lock"))
            .unwrap();
        if blocked_save {
            assert_eq!(
                unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) },
                0
            );
        }
        let start = Instant::now();
        assert_eq!(unsafe { libc::kill(child.id() as i32, libc::SIGINT) }, 0);
        let out = finish(child);
        assert_eq!(out.status.code(), Some(130));
        assert!(start.elapsed() < Duration::from_secs(1));
        assert_eq!(
            unsafe { libc::kill(ready["pid"].as_i64().unwrap() as i32, 0) },
            -1,
            "child server must be reaped"
        );
        let original: Value =
            serde_json::from_slice(&fs::read(home.join(".codex/accounts/alpha.json")).unwrap())
                .unwrap();
        assert_eq!(original["token"], if blocked_save { "old" } else { "new" });
        let recovery = Path::new(ready["auth"].as_str().unwrap());
        if blocked_save {
            assert!(String::from_utf8_lossy(&out.stderr)
                .contains(&format!("preserved at {}", recovery.display())));
            let refreshed: Value = serde_json::from_slice(&fs::read(recovery).unwrap()).unwrap();
            assert_eq!(refreshed["token"], "new");
            assert_eq!(
                fs::metadata(recovery).unwrap().permissions().mode() & 0o777,
                0o600
            );
            assert_eq!(
                fs::metadata(recovery.parent().unwrap())
                    .unwrap()
                    .permissions()
                    .mode()
                    & 0o777,
                0o700
            );
            fs::remove_dir_all(recovery.parent().unwrap()).unwrap();
        } else {
            assert!(!recovery.exists());
        }
    }
    let t = limits_fixture();
    let out = limits(t.path(), "rotate-error").output().unwrap();
    assert!(!out.status.success());
    let original: Value =
        serde_json::from_slice(&fs::read(t.path().join(".codex/accounts/alpha.json")).unwrap())
            .unwrap();
    assert_eq!(
        original["token"], "new",
        "an earlier RPC failure must not discard rotation"
    );
}
#[test]
fn waiting_for_initial_auth_lock_is_cancellable() {
    let t = limits_fixture();
    // Create the lock with the same security policy as the application.
    private_json(&t.path().join(".codex/appserver-ctl-auth.lock"), &json!({}));
    let lock = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(t.path().join(".codex/appserver-ctl-auth.lock"))
        .unwrap();
    assert_eq!(
        unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) },
        0
    );
    let child = limits(t.path(), "ok")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    std::thread::sleep(Duration::from_millis(200));
    let start = Instant::now();
    assert_eq!(unsafe { libc::kill(child.id() as i32, libc::SIGINT) }, 0);
    let out = finish(child);
    assert_eq!(out.status.code(), Some(130));
    assert!(start.elapsed() < Duration::from_secs(1));
    assert!(!t.path().join(".codex/appserver-ctl-limits.json").exists());
}
#[test]
fn cache_follows_user_identity_and_preserves_last_good_timestamp() {
    let t = limits_fixture();
    let home = t.path();
    assert!(limits(home, "ok").output().unwrap().status.success());
    let first = cache(home);
    // A token refresh is still the same user/workspace, so failure should show STALE.
    let mut rotated = auth("alpha", 11);
    rotated["token"] = json!("generation-2");
    rotated["tokens"]["account_id"] = Value::Null;
    private_json(&home.join(".codex/accounts/alpha.json"), &rotated);
    let stale = limits(home, "fail").output().unwrap();
    let text = String::from_utf8_lossy(&stale.stdout);
    assert!(text.contains("11%") && text.contains("STALE"));
    assert_eq!(
        cache(home)["profile:alpha"]["updated_at"],
        first["profile:alpha"]["updated_at"]
    );
    // Different users in the same workspace must never reuse one another's limits.
    private_json(
        &home.join(".codex/accounts/alpha.json"),
        &auth("replacement", 88),
    );
    let out = limits(home, "fail").output().unwrap();
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(
        text.contains("ERROR")
            && !text.contains("11%")
            && !text
                .lines()
                .filter(|line| line.starts_with('|'))
                .any(|line| line.contains("STALE")),
        "{text}"
    );
    assert!(cache(home).get("profile:alpha").is_none());

    // A different workspace also invalidates the cache, even for the same user.
    private_json(&home.join(".codex/accounts/alpha.json"), &auth("alpha", 11));
    assert!(limits(home, "ok").output().unwrap().status.success());
    let mut other_workspace = auth("alpha", 11);
    other_workspace["tokens"]["account_id"] = json!("different-workspace");
    private_json(&home.join(".codex/accounts/alpha.json"), &other_workspace);
    assert!(
        !String::from_utf8_lossy(&limits(home, "fail").output().unwrap().stdout).contains("11%")
    );

    // Missing identity fails closed rather than persisting a name-only cache.
    let mut unknown = auth("alpha", 11);
    unknown["tokens"]["id_token"] = json!("invalid-jwt");
    private_json(&home.join(".codex/accounts/alpha.json"), &unknown);
    assert!(limits(home, "ok").output().unwrap().status.success());
    assert!(cache(home).as_object().unwrap().is_empty());
    assert!(
        !String::from_utf8_lossy(&limits(home, "fail").output().unwrap().stdout).contains("11%")
    );

    private_json(&home.join(".codex/accounts/alpha.json"), &auth("alpha", 11));
    private_json(&home.join("replacement"), &auth("replacement", 88));
    let out = limits(home, "replace").output().unwrap();
    assert!(!out.status.success());
    assert!(!String::from_utf8_lossy(&out.stdout).contains("11%"));
    assert!(String::from_utf8_lossy(&out.stderr).contains("account changed during check"));
    assert!(cache(home).get("profile:alpha").is_none());
}
#[test]
fn profile_named_current_and_unmanaged_active_account_are_distinct() {
    let t = limits_fixture();
    let home = t.path();
    fs::rename(
        home.join(".codex/accounts/alpha.json"),
        home.join(".codex/accounts/current.json"),
    )
    .unwrap();
    private_json(&home.join(".codex/auth.json"), &auth("other", 88));
    let out = limits(home, "ok").output().unwrap();
    assert!(out.status.success());
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(
        text.contains("current")
            && text.contains("(active)")
            && text.contains("11%")
            && text.contains("88%"),
        "{text}"
    );
    assert_eq!(cache(home)["profile:current"]["windows"][0]["used"], 11.);
    assert_eq!(cache(home)["active"]["windows"][0]["used"], 88.);
}

// Real PTY: exercises is_terminal(), dimensions, redraw, and confirmation paths.
fn pty(
    command: &mut Command,
    columns: u16,
    rows: u16,
    confirm: bool,
    resize: bool,
) -> (Output, String) {
    let mut master = 0;
    let mut slave = 0;
    let mut size = libc::winsize {
        ws_row: rows,
        ws_col: columns,
        ws_xpixel: 0,
        ws_ypixel: 0,
    };
    assert_eq!(
        unsafe {
            libc::openpty(
                &mut master,
                &mut slave,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                &mut size,
            )
        },
        0
    );
    let mut master = unsafe { fs::File::from_raw_fd(master) };
    let slave = unsafe { fs::File::from_raw_fd(slave) };
    let mut original: libc::termios = unsafe { std::mem::zeroed() };
    assert_eq!(
        unsafe { libc::tcgetattr(master.as_raw_fd(), &mut original) },
        0
    );
    command
        .stdin(slave.try_clone().unwrap())
        .stdout(slave.try_clone().unwrap())
        .stderr(slave);
    let mut child = command.spawn().unwrap();
    let start = Instant::now();
    let mut text = String::new();
    let mut confirmed = false;
    let mut resized = false;
    let mut watch_prompts = 0;
    loop {
        if start.elapsed() > Duration::from_secs(5) {
            child.kill().unwrap();
            let _ = child.wait();
            panic!("PTY command timed out: {text}");
        }
        let mut poll = libc::pollfd {
            fd: master.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        if unsafe { libc::poll(&mut poll, 1, 30) } > 0 {
            let mut buf = [0; 8192];
            match master.read(&mut buf) {
                Ok(0) | Err(_) => break,
                Ok(n) => text.push_str(&String::from_utf8_lossy(&buf[..n])),
            }
            if confirm && !confirmed && text.contains("Install/update? [y/N]") {
                master.write_all(b"y\n").unwrap();
                confirmed = true;
            }
            if resize && !resized && text.contains("Refreshing weekly limits") {
                size.ws_col = 80;
                size.ws_row = 10;
                assert_eq!(
                    unsafe { libc::ioctl(master.as_raw_fd(), libc::TIOCSWINSZ, &size) },
                    0
                );
                resized = true;
            }
            let prompts = text.matches("[r] refresh").count();
            if prompts > watch_prompts {
                master
                    .write_all(if prompts == 1 { b"r" } else { b"q" })
                    .unwrap();
                watch_prompts = prompts;
            }
        } else if child.try_wait().unwrap().is_some() {
            break;
        }
    }
    let output = child.wait_with_output().unwrap();
    let mut restored: libc::termios = unsafe { std::mem::zeroed() };
    assert_eq!(
        unsafe { libc::tcgetattr(master.as_raw_fd(), &mut restored) },
        0
    );
    // PENDIN is transient kernel state when returning to canonical input on BSD.
    let mode = libc::ICANON | libc::ECHO | libc::ISIG;
    assert_eq!(
        restored.c_lflag & mode,
        original.c_lflag & mode,
        "canonical input, echo, and signal handling must be restored"
    );
    (output, text)
}
#[test]
fn terminal_refresh_keeps_cached_gauge_and_compacts_without_erasing_scrollback() {
    for (columns, rows, resize) in [
        (160, 24, false),
        (80, 24, false),
        (40, 5, false),
        (160, 24, true),
    ] {
        let t = limits_fixture();
        assert!(limits(t.path(), "ok").output().unwrap().status.success());
        private_json(
            &t.path().join(".codex/accounts/alpha.json"),
            &auth("alpha", 22),
        );
        let (out, text) = pty(&mut limits(t.path(), "delay"), columns, rows, false, resize);
        assert!(out.status.success(), "{text}");
        let initial = text.split("Refreshing weekly limits").next().unwrap();
        assert!(
            initial.contains("11%")
                && initial.contains("REFRESHING")
                && (initial.contains("Last updated") || initial.contains("LAST UPDATED")),
            "{text}"
        );
        assert!(
            text.contains("[█████████░] 89%")
                && text.contains("[████████░░] 78%")
                && text.contains("22%")
                && text.contains("Refresh complete"),
            "{text}"
        );
        if rows == 5 || resize {
            assert!(
                !text.contains("A\r\x1b[J"),
                "oversized frame must not erase earlier terminal output"
            );
        }
        for line in initial.lines() {
            assert!(
                line.trim_end_matches('\r').chars().count() <= columns as usize,
                "line does not fit {columns} columns: {line}"
            );
        }
    }
}
#[test]
fn watch_keyboard_refresh_and_quit_restore_terminal() {
    let t = limits_fixture();
    let mut command = limits(t.path(), "delay");
    command.arg("--watch");
    let (out, text) = pty(&mut command, 160, 24, false, false);
    assert!(out.status.success(), "{text}");
    assert_eq!(text.matches("Refresh complete").count(), 2, "{text}");
}
#[test]
fn account_replacement_during_cancellation_hides_old_metrics_and_preserves_rotation() {
    let t = limits_fixture();
    let home = t.path();
    assert!(limits(home, "ok").output().unwrap().status.success());
    std::thread::scope(|scope| {
        let replacing = scope.spawn(|| {
            wait_file(&home.join("ready"));
            let ready: Value =
                serde_json::from_slice(&fs::read(home.join("ready")).unwrap()).unwrap();
            private_json(
                &home.join(".codex/accounts/alpha.json"),
                &auth("replacement", 88),
            );
            assert_eq!(
                unsafe { libc::kill(ready["parent"].as_i64().unwrap() as i32, libc::SIGINT) },
                0
            );
            ready["auth"].as_str().unwrap().to_owned()
        });
        let (out, text) = pty(&mut limits(home, "rotate-hang"), 160, 24, false, false);
        let recovery = replacing.join().unwrap();
        assert_eq!(out.status.code(), Some(130));
        let final_frame = text.rsplit("\x1b[J").next().unwrap();
        assert!(
            final_frame.contains("CANCELLED") && !final_frame.contains("11%"),
            "{text}"
        );
        assert!(text.contains(&format!("preserved at {recovery}")), "{text}");
        let preserved: Value = serde_json::from_slice(&fs::read(&recovery).unwrap()).unwrap();
        assert_eq!(preserved["token"], "new");
        let original: Value =
            serde_json::from_slice(&fs::read(home.join(".codex/accounts/alpha.json")).unwrap())
                .unwrap();
        assert_eq!(original["name"], "replacement");
        fs::remove_dir_all(Path::new(&recovery).parent().unwrap()).unwrap();
    });
}
#[test]
fn ssh_rejects_old_command_semantics_and_rechecks_after_install() {
    let t = tempfile::tempdir().unwrap();
    let home = t.path();
    fs::create_dir(home.join("bin")).unwrap();
    let old_help = "Commands:\n  auth use [NAME]\n  auth list|current\n  start|restart|stop|status\n  update [--no-restart] [--timeout N]\n  usage [daily|monthly|session]\nUsage reads local history. It does not query remaining account limits.\n";
    fs::write(home.join("remote-help"), old_help).unwrap();
    executable(
        &home.join("bin/ssh"),
        r#"#!/usr/bin/env python3
import json, os, sys
root=os.environ['HOME']
with open(os.path.join(root,'ssh-log'),'a') as f: f.write(json.dumps(sys.argv[1:])+'\n')
command=sys.argv[-1]
if '--help' in command:
 file='after-help' if os.path.exists(os.path.join(root,'installed')) and os.path.exists(os.path.join(root,'after-help')) else 'remote-help'
 print(open(os.path.join(root,file)).read())
elif 'sh -s -- --version' in command:
 sys.stdin.read()
 open(os.path.join(root,'installed'),'w').close()
elif 'exec "$ctl"' in command:
 open(os.path.join(root,'forwarded'),'w').write(command)
else: sys.exit(2)
"#,
    );
    for args in [vec!["update"], vec!["update", "codex"], vec!["limits"]] {
        let mut args = args;
        args.extend(["--target", "fake"]);
        let out = cli(home, &args).stdin(Stdio::null()).output().unwrap();
        assert!(!out.status.success());
        assert!(String::from_utf8_lossy(&out.stderr).contains("requires confirmation"));
        assert!(!home.join("forwarded").exists());
    }
    assert!(cli(home, &["status", "--target", "fake"])
        .output()
        .unwrap()
        .status
        .success());
    fs::remove_file(home.join("forwarded")).unwrap();
    let (out, text) = pty(
        &mut cli(home, &["update", "--target", "fake"]),
        160,
        24,
        true,
        false,
    );
    assert!(!out.status.success(), "{text}");
    assert!(
        text.contains("installed remote release does not support"),
        "{text}"
    );
    assert!(!home.join("forwarded").exists());
    let new_help = Command::new(env!("CARGO_BIN_EXE_codex-appserver-ctl"))
        .arg("--help")
        .output()
        .unwrap();
    fs::write(home.join("after-help"), &new_help.stdout).unwrap();
    for args in [vec!["update"], vec!["update", "codex"], vec!["limits"]] {
        let mut args = args;
        args.extend(["--target", "fake"]);
        fs::remove_file(home.join("installed")).unwrap();
        let (out, text) = pty(&mut cli(home, &args), 160, 24, true, false);
        assert!(out.status.success(), "{text}");
        assert!(home.join("forwarded").exists());
        fs::remove_file(home.join("forwarded")).unwrap();
    }
}
