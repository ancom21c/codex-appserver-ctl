use std::{
    fs,
    os::unix::fs::{symlink, PermissionsExt},
    path::Path,
    process::Command,
};
fn exe(path: &Path, contents: &str) {
    fs::write(path, contents).unwrap();
    fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
}
fn fixture() -> tempfile::TempDir {
    let t = tempfile::tempdir().unwrap();
    let root = t.path();
    fs::create_dir(root.join("bin")).unwrap();
    fs::create_dir(root.join("assets")).unwrap();
    fs::create_dir(root.join("package")).unwrap();
    exe(
        &root.join("package/codex-appserver-ctl"),
        "#!/bin/sh\necho 'codex-appserver-ctl TEST'\n",
    );
    for target in [
        "x86_64-unknown-linux-musl",
        "aarch64-unknown-linux-musl",
        "x86_64-apple-darwin",
        "aarch64-apple-darwin",
    ] {
        let asset = format!("codex-appserver-ctl-{target}.tar.gz");
        assert!(Command::new("tar")
            .arg("-czf")
            .arg(root.join("assets").join(&asset))
            .arg("-C")
            .arg(root.join("package"))
            .arg("codex-appserver-ctl")
            .status()
            .unwrap()
            .success());
        let digest = Command::new("shasum")
            .args(["-a", "256", &asset])
            .current_dir(root.join("assets"))
            .output()
            .unwrap();
        assert!(digest.status.success());
        fs::write(
            root.join("assets").join(format!("{asset}.sha256")),
            digest.stdout,
        )
        .unwrap();
    }
    exe(&root.join("bin/uname"), "#!/bin/sh\ncase \"$1\" in -s) echo \"${TEST_OS:-Linux}\" ;; -m) echo \"${TEST_CPU:-x86_64}\" ;; *) exit 1 ;; esac\n");
    exe(
        &root.join("bin/cargo"),
        "#!/bin/sh\necho 'Cargo must not run' >&2\nexit 99\n",
    );
    exe(
        &root.join("bin/curl"),
        r#"#!/bin/sh
while [ "$#" -gt 0 ]; do
  if [ "$1" = --output ]; then shift; out=$1; else url=$1; fi
  shift
done
printf '%s\n' "$url" >> "$TEST_ROOT/urls"
[ "${TEST_DOWNLOAD_FAIL:-0}" = 0 ] || exit 22
cp "$TEST_ROOT/assets/${url##*/}" "$out"
"#,
    );
    t
}
fn install(root: &Path, args: &[&str]) -> Command {
    let mut c = Command::new("sh");
    c.arg(concat!(env!("CARGO_MANIFEST_DIR"), "/install.sh"))
        .arg("--prefix")
        .arg(root.join("install"))
        .args(args)
        .env("TEST_ROOT", root)
        .env(
            "PATH",
            format!("{}:/usr/bin:/bin", root.join("bin").display()),
        );
    c
}
#[test]
fn release_installer_selects_platform_without_cargo_and_keeps_backup() {
    for (os, cpu, target) in [
        ("Linux", "x86_64", "x86_64-unknown-linux-musl"),
        ("Linux", "aarch64", "aarch64-unknown-linux-musl"),
        ("Darwin", "x86_64", "x86_64-apple-darwin"),
        ("Darwin", "arm64", "aarch64-apple-darwin"),
    ] {
        let t = fixture();
        let out = install(t.path(), &["--version", "0.3.0"])
            .env("TEST_OS", os)
            .env("TEST_CPU", cpu)
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        let dest = t.path().join("install/bin/codex-appserver-ctl");
        assert!(dest.is_file());
        assert!(fs::read_to_string(t.path().join("urls"))
            .unwrap()
            .contains(&format!(
                "/download/v0.3.0/codex-appserver-ctl-{target}.tar.gz"
            )));
        fs::write(&dest, "old").unwrap();
        assert!(install(t.path(), &[])
            .env("TEST_OS", os)
            .env("TEST_CPU", cpu)
            .status()
            .unwrap()
            .success());
        let backups: Vec<_> = fs::read_dir(dest.parent().unwrap())
            .unwrap()
            .flatten()
            .filter(|e| e.file_name().to_string_lossy().contains(".backup."))
            .collect();
        assert_eq!(backups.len(), 1);
        assert_eq!(fs::read_to_string(backups[0].path()).unwrap(), "old");
        assert!(install(t.path(), &[])
            .env("TEST_OS", os)
            .env("TEST_CPU", cpu)
            .status()
            .unwrap()
            .success());
        assert_eq!(fs::read_dir(dest.parent().unwrap()).unwrap().count(), 2);
    }
}
#[test]
fn corrupt_download_and_unsafe_destination_do_not_replace_files() {
    let t = fixture();
    fs::create_dir_all(t.path().join("install/bin")).unwrap();
    let dest = t.path().join("install/bin/codex-appserver-ctl");
    fs::write(&dest, "keep").unwrap();
    assert!(!install(t.path(), &[])
        .env("TEST_DOWNLOAD_FAIL", "1")
        .output()
        .unwrap()
        .status
        .success());
    let digest = t
        .path()
        .join("assets/codex-appserver-ctl-x86_64-unknown-linux-musl.tar.gz.sha256");
    fs::write(&digest, "0".repeat(64)).unwrap();
    assert!(!install(t.path(), &[]).output().unwrap().status.success());
    assert_eq!(fs::read_to_string(&dest).unwrap(), "keep");
    fs::remove_file(&dest).unwrap();
    symlink(t.path().join("package/codex-appserver-ctl"), &dest).unwrap();
    let out = install(
        t.path(),
        &[
            "--binary",
            t.path()
                .join("package/codex-appserver-ctl")
                .to_str()
                .unwrap(),
        ],
    )
    .output()
    .unwrap();
    assert!(!out.status.success());
    assert!(dest.is_symlink());
}
#[test]
fn unsupported_cpu_and_conflicting_options_fail_before_download() {
    let t = fixture();
    assert!(!install(t.path(), &[])
        .env("TEST_CPU", "riscv64")
        .output()
        .unwrap()
        .status
        .success());
    assert!(!install(t.path(), &["--source", "--version", "0.3.0"])
        .output()
        .unwrap()
        .status
        .success());
    assert!(!t.path().join("urls").exists());
}
