mod limits;
mod usage;
use serde_json::Value;
use std::{
    collections::{BTreeSet, HashMap},
    env,
    error::Error,
    fs::{self, File, OpenOptions},
    io::{self, IsTerminal, Read, Seek, SeekFrom, Write},
    os::unix::{
        fs::{symlink, MetadataExt, OpenOptionsExt, PermissionsExt},
        process::CommandExt,
    },
    path::{Path, PathBuf},
    process::{Command, Stdio},
    thread,
    time::{Duration, Instant},
};
type Result<T> = std::result::Result<T, Box<dyn Error>>;
fn err(message: impl Into<String>) -> Box<dyn Error> {
    message.into().into()
}
const INSTALL_URL: &str = "https://chatgpt.com/codex/install.sh";
const HELP: &str = "Control Codex locally on macOS/Linux or over SSH.\n\nCommands:\n  auth list|current\n  auth login NAME [--force] [--timeout N]\n  auth save NAME [--force] [--dry-run]\n  save NAME [--force] [--dry-run]\n  auth use [NAME] [--no-restart] [--dry-run]\n  start|restart|stop|status\n  remote-control start|stop|pair|enable|disable|status|bootstrap\n  update [--timeout N] [--dry-run]\n  update codex [--no-restart] [--timeout N] [--dry-run]\n  limits [--timeout N] [--watch]\n  usage [daily|monthly|session] [--json] [--since DATE] [--until DATE] [--last DAYS] [--prices FILE]\n  doctor\n  targets\n  --version\n  logs [--follow] [--lines N] [--file PATH|--unit UNIT]\n\nAdd --target MY_SERVER to execute through SSH. Omit it for the current host.\nRemote installation requires confirmation. Remote binary installation requires curl, tar, and a SHA-256 tool.\nUsage reads local history. It does not query remaining account limits.\n";
struct App {
    home: PathBuf,
    data: PathBuf,
    uid: u32,
    binary: Option<PathBuf>,
}
impl App {
    fn new() -> Result<Self> {
        let home = PathBuf::from(env::var("HOME")?);
        let data = home.join(".codex");
        Ok(Self {
            home,
            data,
            uid: unsafe { libc::getuid() },
            binary: None,
        })
    }
    fn codex(&self) -> Result<PathBuf> {
        if let Some(p) = &self.binary {
            return Ok(p.clone());
        }
        for p in [
            env::var_os("CODEX_APPSERVER_HOST_CODEX").map(PathBuf::from),
            which("codex"),
            Some(self.data.join("packages/standalone/current/codex")),
        ]
        .into_iter()
        .flatten()
        {
            if executable(&p) {
                return Ok(p);
            }
        }
        Err(err("Codex executable not found"))
    }
    fn command(&self, args: &[&str]) -> Result<Command> {
        let mut c = Command::new(self.codex()?);
        c.args(args)
            .env("HOME", &self.home)
            .env("CODEX_HOME", &self.data);
        Ok(c)
    }
    fn daemon(&self, action: &str, timeout: u64) -> Result<String> {
        let mut c = self.command(&["app-server", "daemon", action])?;
        capture(&mut c, timeout)
    }
    fn state(&self) -> Option<Value> {
        self.daemon("version", 5)
            .ok()
            .and_then(|s| serde_json::from_str(&s).ok())
    }
    fn validate(&self) -> Result<()> {
        let m = fs::symlink_metadata(&self.data)?;
        if !m.is_dir() || m.uid() != self.uid {
            return Err(err("Codex home must be an owned directory, not a symlink"));
        }
        Ok(())
    }
    fn accounts(&self) -> PathBuf {
        self.data.join("accounts")
    }
    fn secure(&self, p: &Path) -> bool {
        let Ok(m) = fs::symlink_metadata(p) else {
            return false;
        };
        m.is_file()
            && m.uid() == self.uid
            && m.mode() & 0o777 == 0o600
            && m.nlink() == 1
            && m.len() <= 1024 * 1024
            && fs::read(p)
                .ok()
                .and_then(|b| serde_json::from_slice::<Value>(&b).ok())
                .is_some_and(|v| v.as_object().is_some_and(|m| !m.is_empty()))
    }
    fn prepare(&self) -> Result<()> {
        self.validate()?;
        let p = self.accounts();
        if p.exists() || p.is_symlink() {
            let m = fs::symlink_metadata(&p)?;
            if !m.is_dir() || m.uid() != self.uid {
                return Err(err("unsafe account store"));
            }
        } else {
            fs::create_dir(&p)?;
        }
        fs::set_permissions(p, fs::Permissions::from_mode(0o700))?;
        Ok(())
    }
    fn profiles(&self) -> Result<Vec<String>> {
        if !self.accounts().exists() {
            return Ok(vec![]);
        }
        let m = fs::symlink_metadata(self.accounts())?;
        if !m.is_dir() || m.uid() != self.uid {
            return Err(err("unsafe account store"));
        }
        let mut names = vec![];
        for e in fs::read_dir(self.accounts())? {
            let p = e?.path();
            if p.extension().is_some_and(|x| x == "json") && self.secure(&p) {
                if let Some(n) = p.file_stem().and_then(|x| x.to_str()) {
                    if name(n).is_ok() {
                        names.push(n.into());
                    }
                }
            }
        }
        names.sort();
        Ok(names)
    }
    fn active(&self) -> Result<Option<PathBuf>> {
        self.validate()?;
        let p = self.data.join("auth.json");
        let m = match fs::symlink_metadata(&p) {
            Ok(m) => m,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(e.into()),
        };
        if m.file_type().is_symlink() {
            let target = fs::canonicalize(&p)?;
            if target.parent() != Some(fs::canonicalize(self.accounts())?.as_path())
                || !self.secure(&target)
            {
                return Err(err("unsafe active profile link"));
            }
            Ok(Some(target))
        } else if self.secure(&p) {
            Ok(Some(p))
        } else {
            Err(err("auth.json must be an owned 0600 JSON file"))
        }
    }
    fn current(&self) -> Result<String> {
        let Some(p) = self.active()? else {
            return Ok("target=home current=none".into());
        };
        if self.data.join("auth.json").is_symlink() {
            return Ok(format!(
                "target=home current={} source=managed-link",
                p.file_stem().unwrap().to_string_lossy()
            ));
        }
        let b = fs::read(p)?;
        let matches: Vec<_> = self
            .profiles()?
            .into_iter()
            .filter(|n| fs::read(self.accounts().join(format!("{n}.json"))).is_ok_and(|v| v == b))
            .collect();
        Ok(format!(
            "target=home current={} source=content",
            match matches.len() {
                1 => matches[0].clone(),
                0 => "unmanaged".into(),
                _ => "ambiguous".into(),
            }
        ))
    }
    fn lock(&self, timeout: u64) -> Result<File> {
        self.validate()?;
        let p = self.data.join("appserver-ctl-auth.lock");
        let f = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW)
            .open(p)?;
        let m = f.metadata()?;
        if !m.is_file() || m.uid() != self.uid || m.mode() & 0o777 != 0o600 || m.nlink() != 1 {
            return Err(err("unsafe auth lock"));
        }
        let start = Instant::now();
        loop {
            if unsafe {
                libc::flock(
                    std::os::fd::AsRawFd::as_raw_fd(&f),
                    libc::LOCK_EX | libc::LOCK_NB,
                )
            } == 0
            {
                return Ok(f);
            }
            if start.elapsed() > Duration::from_secs(timeout) {
                return Err(err("timed out waiting for auth lock"));
            }
            thread::sleep(Duration::from_millis(50));
        }
    }
    fn check_destination(&self, p: &Path, force: bool) -> Result<()> {
        if self.accounts().exists() || self.accounts().is_symlink() {
            let m = fs::symlink_metadata(self.accounts())?;
            if !m.is_dir() || m.uid() != self.uid {
                return Err(err("unsafe account store"));
            }
        }
        if p.exists() || p.is_symlink() {
            if !self.secure(p) {
                return Err(err("existing profile is unsafe"));
            }
            if !force {
                return Err(err("profile already exists; use --force to overwrite"));
            }
            let auth = self.data.join("auth.json");
            if auth.is_symlink() && fs::canonicalize(auth)? == fs::canonicalize(p)? {
                return Err(err(
                    "cannot overwrite the directly active profile; use a new name",
                ));
            }
        }
        Ok(())
    }
    fn auth_save(&self, n: &str, o: &Options) -> Result<()> {
        let n = name(n)?;
        let p = self.accounts().join(format!("{n}.json"));
        let source = self.active()?.ok_or_else(|| err("no auth.json to save"))?;
        if self.secure(&p) && fs::read(&source)? == fs::read(&p)? {
            println!("auth_save profile={n} result=unchanged");
            return Ok(());
        }
        self.check_destination(&p, o.force)?;
        if o.dry {
            println!("dry-run auth_save profile={n}");
            return Ok(());
        }
        let _lock = self.lock(o.timeout)?;
        self.check_destination(&p, o.force)?;
        self.prepare()?;
        let source = self
            .active()?
            .ok_or_else(|| err("auth disappeared while waiting for lock"))?;
        atomic(&p, &fs::read(source)?)?;
        println!("auth_save profile={n} result=saved");
        Ok(())
    }
    fn auth_login(&self, n: &str, o: &Options) -> Result<()> {
        self.validate()?;
        let n = name(n)?;
        let p = self.accounts().join(format!("{n}.json"));
        self.check_destination(&p, o.force)?;
        if o.dry {
            println!("dry-run auth_login profile={n} active=unchanged");
            return Ok(());
        }
        let temp = tempfile::tempdir()?;
        let mut c = self.command(&[
            "-c",
            "cli_auth_credentials_store=\"file\"",
            "login",
            "--device-auth",
        ])?;
        c.env("CODEX_HOME", temp.path());
        run(&mut c, o.timeout)?;
        let source = temp.path().join("auth.json");
        if !self.secure(&source) {
            return Err(err("login did not produce a valid owned 0600 auth.json"));
        }
        let _lock = self.lock(o.timeout)?;
        self.check_destination(&p, o.force)?;
        self.prepare()?;
        atomic(&p, &fs::read(source)?)?;
        println!(
            "auth_login profile={n} result=saved active=unchanged; use auth use {n} to activate"
        );
        Ok(())
    }
    fn auth_use(&self, n: Option<&str>, o: &Options) -> Result<()> {
        self.validate()?;
        let n = match n {
            Some(n) => name(n)?,
            None => {
                if !io::stdin().is_terminal() {
                    return Err(err("auth use requires NAME outside a terminal"));
                }
                let names = self.profiles()?;
                for (i, n) in names.iter().enumerate() {
                    println!("{}) {n}", i + 1)
                }
                let s = prompt("Profile number: ")?;
                let i: usize = s.parse()?;
                names
                    .get(i.checked_sub(1).ok_or_else(|| err("invalid selection"))?)
                    .cloned()
                    .ok_or_else(|| err("invalid selection"))?
            }
        };
        let profile = self.accounts().join(format!("{n}.json"));
        if !self.secure(&profile) {
            return Err(err("profile must be an owned 0600 JSON file"));
        }
        self.active()?;
        if o.dry {
            println!("dry-run auth_use profile={n} restart={}", o.restart);
            if o.restart {
                self.restart_if_running(o)?
            }
            return Ok(());
        }
        if o.restart && !o.internal && self.within()? {
            return self.schedule(&["auth", "use", &n], o);
        }
        let _lock = self.lock(o.timeout)?;
        self.prepare()?;
        if !self.secure(&profile) {
            return Err(err("profile changed while waiting for lock"));
        }
        let auth = self.data.join("auth.json");
        let current = self.data.join("current");
        let old_auth = Snapshot::read(&auth)?;
        let old_current = Snapshot::read(&current)?;
        let result = (|| {
            atomic_link(&fs::canonicalize(&profile)?, &auth)?;
            atomic(&current, format!("{n}\n").as_bytes())?;
            if o.restart {
                self.restart_if_running(o)?
            } else {
                eprintln!("warning: a running app-server may retain prior auth")
            }
            Ok(())
        })();
        if let Err(e) = result {
            let a = old_auth.restore(&auth);
            let b = old_current.restore(&current);
            if a.is_err() || b.is_err() {
                return Err(err(format!("{e}; auth rollback failed")));
            }
            return Err(e);
        }
        println!("auth_use profile={n} result=switched");
        Ok(())
    }
    fn processes(&self) -> Result<Vec<Proc>> {
        let text = capture(
            Command::new("/bin/ps").args(["-axo", "pid=,ppid=,uid=,command="]),
            5,
        )?;
        Ok(text
            .lines()
            .filter_map(|line| {
                let mut parts = line.split_whitespace();
                let pid = parts.next()?.parse().ok()?;
                let parent = parts.next()?.parse().ok()?;
                let uid: u32 = parts.next()?.parse().ok()?;
                if uid != self.uid {
                    return None;
                }
                Some(Proc {
                    pid,
                    parent,
                    command: parts.collect::<Vec<_>>().join(" "),
                })
            })
            .collect())
    }
    fn servers(&self, rows: &[Proc]) -> Vec<Proc> {
        let selected = self.codex().ok().and_then(|p| fs::canonicalize(p).ok());
        rows.iter()
            .filter(|p| {
                let b = p.command.split_whitespace().next().unwrap_or("");
                let app = b.starts_with("/Applications/ChatGPT.app/")
                    || b.starts_with("/Applications/Codex.app/");
                let local = b.ends_with("/codex")
                    && (b.starts_with(self.data.to_string_lossy().as_ref())
                        || b.starts_with(self.home.join(".local/").to_string_lossy().as_ref()));
                (app || local
                    || selected
                        .as_ref()
                        .is_some_and(|s| fs::canonicalize(b).is_ok_and(|v| v == *s)))
                    && p.command.split_whitespace().any(|w| w == "app-server")
                    && !p.command.contains("app-server proxy")
            })
            .cloned()
            .collect()
    }
    fn within(&self) -> Result<bool> {
        let rows = self.processes()?;
        let servers = self.servers(&rows);
        let parents: HashMap<_, _> = rows.iter().map(|p| (p.pid, p.parent)).collect();
        let mut pid = std::process::id() as i32;
        let mut seen = BTreeSet::new();
        while pid > 1 && seen.insert(pid) {
            if servers.iter().any(|s| s.pid == pid) {
                return Ok(true);
            }
            pid = *parents.get(&pid).unwrap_or(&0);
        }
        Ok(false)
    }
    fn schedule(&self, args: &[&str], o: &Options) -> Result<()> {
        self.validate()?;
        let log = tempfile::Builder::new()
            .prefix("appserver-ctl-detached.")
            .suffix(".log")
            .tempfile_in(&self.data)?;
        let log = log.keep()?.1;
        let mut extra = vec![
            "--internal-direct".to_string(),
            "--timeout".into(),
            o.timeout.to_string(),
        ];
        if o.force {
            extra.push("--force".into())
        }
        let exe = env::current_exe()?;
        if cfg!(target_os = "linux") && which("systemd-run").is_some() {
            let mut c = Command::new("systemd-run");
            c.args(["--user", "--collect", "--on-active=1s", "--quiet"])
                .arg(format!(
                    "--property=StandardOutput=append:{}",
                    log.display()
                ))
                .arg(format!("--property=StandardError=append:{}", log.display()))
                .arg(&exe)
                .args(args)
                .args(&extra);
            if run(&mut c, 10).is_ok() {
                println!("scheduled launcher=systemd-run log={}", log.display());
                return Ok(());
            }
        }
        let out = OpenOptions::new().append(true).open(&log)?;
        let mut c = Command::new(exe);
        c.args(args)
            .args(extra)
            .stdin(Stdio::null())
            .stdout(out.try_clone()?)
            .stderr(out);
        unsafe {
            c.pre_exec(|| {
                if libc::setsid() < 0 {
                    return Err(io::Error::last_os_error());
                }
                Ok(())
            });
        }
        c.spawn()?;
        println!("scheduled launcher=setsid log={}", log.display());
        Ok(())
    }
    fn status(&self) -> Result<()> {
        if let Some(v) = self.state() {
            println!("{}", serde_json::to_string_pretty(&v)?);
            return Ok(());
        }
        let rows = self.processes()?;
        let servers = self.servers(&rows);
        println!(
            "target=home state={} pids={}",
            if servers.is_empty() {
                "not-running"
            } else {
                "running"
            },
            servers
                .iter()
                .map(|p| p.pid.to_string())
                .collect::<Vec<_>>()
                .join(",")
        );
        Ok(())
    }
    fn restart_if_running(&self, o: &Options) -> Result<()> {
        let managed = self.state().is_some_and(|v| v["status"] == "running");
        if !managed && self.servers(&self.processes()?).is_empty() {
            println!("auth_restart result=skipped reason=no-running-app-server");
            return Ok(());
        }
        self.control("restart", o)
    }
    fn control(&self, action: &str, o: &Options) -> Result<()> {
        if o.dry {
            println!("dry-run action={action}");
            return Ok(());
        }
        if !o.internal && action != "start" && self.within()? {
            return self.schedule(&[action], o);
        }
        if cfg!(target_os = "linux")
            || action == "start"
            || self.state().is_some_and(|v| v["status"] == "running")
        {
            println!("{}", self.daemon(action, o.timeout)?);
            return Ok(());
        }
        let rows = self.processes()?;
        let servers = self.servers(&rows);
        if servers.is_empty() {
            if action == "stop" {
                return Ok(());
            }
            return Err(err("no running app-server; use start"));
        }
        let mut targets = BTreeSet::new();
        let mut apps = BTreeSet::new();
        for s in servers {
            let parent = rows.iter().find(|p| p.pid == s.parent);
            let app = parent.and_then(|p| app_name(&p.command));
            if let Some(app) = app {
                targets.insert(s.parent);
                apps.insert(app);
            } else {
                if action == "restart" {
                    return Err(err("cannot restart an unmanaged standalone server"));
                }
                targets.insert(s.pid);
            }
        }
        for pid in &targets {
            unsafe {
                libc::kill(*pid, libc::SIGTERM);
            }
        }
        let wait = |seconds| {
            let start = Instant::now();
            while start.elapsed() < Duration::from_secs(seconds) {
                if targets.iter().all(|p| unsafe { libc::kill(*p, 0) } != 0) {
                    return true;
                }
                thread::sleep(Duration::from_millis(100));
            }
            false
        };
        if !wait(o.timeout.min(15)) {
            if !o.force {
                return Err(err("graceful stop failed; use --force to permit SIGKILL"));
            }
            for pid in &targets {
                unsafe {
                    libc::kill(*pid, libc::SIGKILL);
                }
            }
            if !wait(10) {
                return Err(err("app-server still running"));
            }
        }
        if action == "restart" {
            for app in apps {
                run(Command::new("/usr/bin/open").args(["-a", app]), o.timeout)?;
            }
        }
        Ok(())
    }
    fn remote_control(&self, action: &str, o: &Options) -> Result<()> {
        let args = match action {
            "start" => vec!["remote-control", "start"],
            "stop" => vec!["remote-control", "stop"],
            "pair" => vec!["remote-control", "pair"],
            "enable" => vec!["app-server", "daemon", "enable-remote-control"],
            "disable" => vec!["app-server", "daemon", "disable-remote-control"],
            "status" => vec!["app-server", "daemon", "version"],
            "bootstrap" => vec!["app-server", "daemon", "bootstrap", "--remote-control"],
            _ => return Err(err("unknown remote-control command")),
        };
        if o.dry {
            println!("dry-run codex {}", args.join(" "));
            return Ok(());
        }
        if !o.internal && action != "pair" && action != "status" && self.within()? {
            return self.schedule(&["remote-control", action], o);
        }
        run(&mut self.command(&args)?, o.timeout)
    }
    fn update_self(&self, o: &Options) -> Result<()> {
        const URL: &str =
            "https://github.com/ancom21c/codex-appserver-ctl/releases/latest/download/install.sh";
        let executable = env::current_exe()?;
        let bin = executable
            .parent()
            .ok_or_else(|| err("missing executable directory"))?;
        if bin.file_name().is_none_or(|n| n != "bin") {
            return Err(err("self-update requires installation in PREFIX/bin; use install.sh --binary for a checkout"));
        }
        let prefix = bin.parent().ok_or_else(|| err("missing install prefix"))?;
        if o.dry {
            println!("dry-run installer={URL} prefix={}", prefix.display());
            return Ok(());
        }
        let temp = tempfile::tempdir()?;
        let script = temp.path().join("install.sh");
        run(
            Command::new("curl")
                .args([
                    "--fail",
                    "--silent",
                    "--show-error",
                    "--location",
                    "--proto",
                    "=https",
                    "--proto-redir",
                    "=https",
                    "--connect-timeout",
                    "10",
                    "--max-time",
                    "60",
                    "--output",
                ])
                .arg(&script)
                .arg(URL),
            o.timeout,
        )?;
        run(
            Command::new("sh")
                .arg(&script)
                .arg("--prefix")
                .arg(prefix)
                .env("HOME", &self.home),
            o.timeout,
        )?;
        println!(
            "{}",
            capture(Command::new(&executable).arg("--version"), 10)?.trim()
        );
        Ok(())
    }
    fn update_codex(&mut self, o: &Options) -> Result<()> {
        if o.dry {
            println!(
                "dry-run installer={INSTALL_URL} destination={} restart={}",
                self.home.join(".local/bin/codex").display(),
                o.restart
            );
            return Ok(());
        }
        if o.restart && !o.internal && self.within()? {
            return self.schedule(&["update", "codex"], o);
        }
        let temp = tempfile::tempdir()?;
        let path = temp.path().join("install.sh");
        run(
            Command::new("curl")
                .args([
                    "--fail",
                    "--silent",
                    "--show-error",
                    "--location",
                    "--proto",
                    "=https",
                    "--proto-redir",
                    "=https",
                    "--connect-timeout",
                    "10",
                    "--max-time",
                    "60",
                    "--output",
                ])
                .arg(&path)
                .arg(INSTALL_URL),
            o.timeout,
        )
        .map_err(|e| err(format!("download failed; no restart requested: {e}")))?;
        run(
            Command::new("sh")
                .arg(&path)
                .env("HOME", &self.home)
                .env("CODEX_HOME", &self.data)
                .env("CODEX_INSTALL_DIR", self.home.join(".local/bin"))
                .env("CODEX_NON_INTERACTIVE", "1")
                .env("CODEX_RELEASE", "latest")
                .env("CODEX_INSTALL_DAEMON_ONLY", "0"),
            o.timeout,
        )
        .map_err(|e| err(format!("installer failed; no restart requested: {e}")))?;
        let binary = self.home.join(".local/bin/codex");
        let version = capture(Command::new(&binary).arg("--version"), 10)?;
        println!(
            "codex_update executable={} version={}",
            binary.display(),
            version.trim()
        );
        self.binary = Some(binary);
        if o.restart {
            self.restart_if_running(o)
                .map_err(|e| err(format!("CLI installed but restart failed: {e}")))?
        }
        println!(
            "Put {} first in PATH.",
            self.home.join(".local/bin").display()
        );
        Ok(())
    }
    fn doctor(&self) -> Result<()> {
        let mut failed = false;
        let mut check = |label: &str, r: Result<String>| match r {
            Ok(s) => println!("OK {label}: {s}"),
            Err(e) => {
                failed = true;
                println!("FAIL {label}: {e}")
            }
        };
        check(
            "CLI",
            self.command(&["--version"])
                .and_then(|mut c| capture(&mut c, 10)),
        );
        check("auth", self.current());
        check(
            "profiles",
            self.profiles().and_then(|n| {
                let mut files = 0;
                if self.accounts().exists() {
                    for entry in fs::read_dir(self.accounts())? {
                        if entry?.path().extension().is_some_and(|e| e == "json") {
                            files += 1;
                        }
                    }
                }
                if files != n.len() {
                    return Err(err(format!("{} invalid profile files", files - n.len())));
                }
                Ok(format!("{} valid profiles", n.len()))
            }),
        );
        for args in [
            vec!["app-server", "daemon", "--help"],
            vec!["remote-control", "--help"],
            vec!["app-server", "daemon", "bootstrap", "--help"],
            vec!["login", "--help"],
        ] {
            check(
                &args[..args.len() - 1].join(" "),
                self.command(&args)
                    .and_then(|mut c| capture(&mut c, 10))
                    .map(|_| "supported".into()),
            );
        }
        if let Some(v) = self.state() {
            println!("OK daemon: {}", v["status"])
        } else {
            println!("WARN daemon: unavailable; macOS may use an app-hosted server")
        }
        if failed {
            return Err(err("doctor found failed checks"));
        }
        Ok(())
    }
}
#[derive(Clone)]
struct Proc {
    pid: i32,
    parent: i32,
    command: String,
}
fn app_name(command: &str) -> Option<&'static str> {
    if command.starts_with("/Applications/ChatGPT.app/") {
        Some("ChatGPT")
    } else if command.starts_with("/Applications/Codex.app/") {
        Some("Codex")
    } else {
        None
    }
}
#[derive(Debug)]
struct Options {
    pos: Vec<String>,
    force: bool,
    dry: bool,
    restart: bool,
    internal: bool,
    timeout: u64,
}
fn options(args: &[String]) -> Result<Options> {
    let mut o = Options {
        pos: vec![],
        force: false,
        dry: false,
        restart: true,
        internal: false,
        timeout: 120,
    };
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--force" => o.force = true,
            "--dry-run" => o.dry = true,
            "--no-restart" => o.restart = false,
            "--restart" => o.restart = true,
            "--internal-direct" => o.internal = true,
            "--timeout" => {
                i += 1;
                o.timeout = args
                    .get(i)
                    .ok_or_else(|| err("--timeout requires a value"))?
                    .parse()?
            }
            s if s.starts_with("--timeout=") => o.timeout = s[10..].parse()?,
            s if s.starts_with("--restart=") => o.restart = boolean(&s[10..])?,
            s if s.starts_with('-') => return Err(err(format!("unknown option: {s}"))),
            _ => o.pos.push(args[i].clone()),
        }
        i += 1;
    }
    if !(1..=900).contains(&o.timeout) {
        return Err(err("timeout must be between 1 and 900"));
    }
    Ok(o)
}
fn boolean(s: &str) -> Result<bool> {
    match s {
        "true" | "1" | "yes" | "on" => Ok(true),
        "false" | "0" | "no" | "off" => Ok(false),
        _ => Err(err("expected true or false")),
    }
}
fn name(n: &str) -> Result<String> {
    let n = n.strip_suffix(".json").unwrap_or(n);
    if n.is_empty()
        || n.len() > 128
        || !n.as_bytes()[0].is_ascii_alphanumeric()
        || !n
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"._-".contains(&b))
    {
        return Err(err("invalid profile name"));
    }
    Ok(n.into())
}
fn executable(p: &Path) -> bool {
    fs::metadata(p).is_ok_and(|m| m.is_file() && m.mode() & 0o111 != 0)
}
fn which(n: &str) -> Option<PathBuf> {
    env::split_paths(&env::var_os("PATH")?)
        .map(|p| p.join(n))
        .find(|p| executable(p))
}
fn wait(c: &mut Command, timeout: u64) -> Result<()> {
    let mut child = c.spawn()?;
    let start = Instant::now();
    loop {
        if let Some(s) = child.try_wait()? {
            return if s.success() {
                Ok(())
            } else {
                Err(err(format!("command exited with {s}")))
            };
        }
        if start.elapsed() > Duration::from_secs(timeout) {
            child.kill()?;
            child.wait()?;
            return Err(err("command timed out"));
        }
        thread::sleep(Duration::from_millis(25));
    }
}
fn run(c: &mut Command, timeout: u64) -> Result<()> {
    wait(c, timeout)
}
fn capture(c: &mut Command, timeout: u64) -> Result<String> {
    let mut out = tempfile::tempfile()?;
    c.stdout(out.try_clone()?).stderr(out.try_clone()?);
    let r = wait(c, timeout);
    out.seek(SeekFrom::Start(0))?;
    let mut text = String::new();
    out.read_to_string(&mut text)?;
    r.map_err(|e| err(format!("{e}: {}", text.trim())))?;
    Ok(text)
}
fn atomic(p: &Path, b: &[u8]) -> Result<()> {
    let mut temp =
        tempfile::NamedTempFile::new_in(p.parent().ok_or_else(|| err("missing parent"))?)?;
    temp.as_file()
        .set_permissions(fs::Permissions::from_mode(0o600))?;
    temp.write_all(b)?;
    temp.as_file().sync_all()?;
    temp.persist(p)?;
    Ok(())
}
fn atomic_link(target: &Path, p: &Path) -> Result<()> {
    let temp = tempfile::tempdir_in(p.parent().unwrap())?;
    let link = temp.path().join("link");
    symlink(target, &link)?;
    fs::rename(link, p)?;
    Ok(())
}
enum Snapshot {
    Missing,
    File(Vec<u8>),
    Link(PathBuf),
}
impl Snapshot {
    fn read(p: &Path) -> Result<Self> {
        match fs::symlink_metadata(p) {
            Ok(m) if m.file_type().is_symlink() => Ok(Self::Link(fs::read_link(p)?)),
            Ok(m) if m.is_file() => Ok(Self::File(fs::read(p)?)),
            Ok(_) => Err(err("unsafe transaction path")),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(Self::Missing),
            Err(e) => Err(e.into()),
        }
    }
    fn restore(self, p: &Path) -> Result<()> {
        match self {
            Self::Missing => {
                match fs::remove_file(p) {
                    Ok(()) => (),
                    Err(e) if e.kind() == io::ErrorKind::NotFound => (),
                    Err(e) => return Err(e.into()),
                }
                Ok(())
            }
            Self::File(b) => atomic(p, &b),
            Self::Link(t) => atomic_link(&t, p),
        }
    }
}
fn prompt(s: &str) -> Result<String> {
    print!("{s}");
    io::stdout().flush()?;
    let mut answer = String::new();
    io::stdin().read_line(&mut answer)?;
    Ok(answer.trim().into())
}
fn target(args: &[String]) -> Result<(Option<String>, Vec<String>)> {
    let mut host = None;
    let mut rest = vec![];
    let mut i = 0;
    while i < args.len() {
        let a = &args[i];
        if a == "--target" || a.starts_with("--target=") {
            if host.is_some() {
                return Err(err("--target specified more than once"));
            }
            let s = if a == "--target" {
                i += 1;
                args.get(i)
                    .ok_or_else(|| err("--target requires SSH destination"))?
                    .clone()
            } else {
                a[9..].into()
            };
            if s.is_empty()
                || s.starts_with('-')
                || s.chars().any(|c| c.is_whitespace() || c.is_control())
            {
                return Err(err("invalid SSH destination"));
            }
            host = Some(s);
        } else {
            rest.push(a.clone())
        }
        i += 1;
    }
    Ok((host, rest))
}
fn quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}
fn join(args: &[String]) -> String {
    args.iter().map(|s| quote(s)).collect::<Vec<_>>().join(" ")
}
fn remote(host: &str, args: &[String]) -> Result<()> {
    let lookup =
        "ctl=$(command -v codex-appserver-ctl) || ctl=\"$HOME/.local/bin/codex-appserver-ctl\"; ";
    let probe = Command::new("ssh")
        .args([
            "--",
            host,
            &format!("{lookup}[ -x \"$ctl\" ] || exit 127; \"$ctl\" --help"),
        ])
        .output()?;
    let help = String::from_utf8_lossy(&probe.stdout);
    let modern = help.contains("auth use [NAME]");
    let needed =
        if args.first().is_some_and(|s| s == "auth") && args.get(1).is_some_and(|s| s == "login") {
            "auth login"
        } else {
            args.first().map(String::as_str).unwrap_or("")
        };
    let supported = probe.status.success()
        && (needed.is_empty()
            || needed == "--help"
            || needed == "-h"
            || help.contains(needed)
            || matches!(needed, "true" | "false" | "restart" | "stop"));
    let mut installed = false;
    if !supported {
        if !probe.status.success() && probe.status.code() != Some(127) {
            io::stderr().write_all(&probe.stderr)?;
            return Err(err(format!("SSH probe failed: {}", probe.status)));
        }
        eprintln!("Tool on {host} is missing or does not support this command.");
        if args.iter().any(|s| s == "--dry-run") {
            return Err(err(
                "remote installation required; dry-run does not install",
            ));
        }
        if !io::stdin().is_terminal() {
            return Err(err(
                "remote installation requires confirmation in an interactive terminal",
            ));
        }
        eprintln!("Download and install version {} on {host} at ~/.local/bin/codex-appserver-ctl?\nRequires curl, tar, a SHA-256 tool, and network access on the target. Existing file will be backed up.\nAfter installation the requested command will run.", env!("CARGO_PKG_VERSION"));
        if !matches!(
            prompt("Install/update? [y/N] ")?.to_lowercase().as_str(),
            "y" | "yes"
        ) {
            return Err(err("installation declined; no changes"));
        }
        install_remote(host)?;
        installed = true;
    }
    let mut forwarded = args.to_vec();
    if !installed && !modern {
        match forwarded.first().map(String::as_str) {
            Some("auth") => forwarded.insert(2, "home".into()),
            Some("status") => forwarded.insert(1, "home".into()),
            Some("restart") => {
                forwarded[0] = "true".into();
                forwarded.insert(0, "home".into());
            }
            Some("stop") => {
                forwarded[0] = "false".into();
                forwarded.insert(0, "home".into());
            }
            Some("true" | "false") => forwarded.insert(0, "home".into()),
            _ => {}
        }
    }
    let lookup = if installed {
        "ctl=\"$HOME/.local/bin/codex-appserver-ctl\"; "
    } else {
        lookup
    };
    let command = format!("{lookup}exec \"$ctl\" {}", join(&forwarded));
    let mut c = Command::new("ssh");
    if io::stdin().is_terminal() && io::stdout().is_terminal() {
        c.arg("-t");
    }
    c.args(["--", host, &command]);
    let status = c.status()?;
    if !status.success() {
        std::process::exit(status.code().unwrap_or(1));
    }
    Ok(())
}
fn install_remote(host: &str) -> Result<()> {
    let mut script = tempfile::tempfile()?;
    script.write_all(include_bytes!("../install.sh"))?;
    script.seek(SeekFrom::Start(0))?;
    run(
        Command::new("ssh")
            .args([
                "-T",
                "--",
                host,
                &format!("sh -s -- --version {}", quote(env!("CARGO_PKG_VERSION"))),
            ])
            .stdin(script),
        900,
    )
}
fn ssh_words(line: &str) -> Vec<String> {
    let mut words = Vec::new();
    let mut word = String::new();
    let mut quote = None;
    let mut escape = false;
    for c in line.chars() {
        if escape {
            word.push(c);
            escape = false;
            continue;
        }
        if c == '\\' {
            escape = true;
            continue;
        }
        if let Some(q) = quote {
            if c == q {
                quote = None;
            } else {
                word.push(c);
            }
        } else if c == '#' {
            break;
        } else if c == '\'' || c == '"' {
            quote = Some(c);
        } else if c.is_whitespace() || (c == '=' && words.is_empty()) {
            if !word.is_empty() {
                words.push(std::mem::take(&mut word));
            }
        } else {
            word.push(c);
        }
    }
    if !word.is_empty() {
        words.push(word);
    }
    words
}
fn aliases(home: &Path) -> Result<Vec<String>> {
    fn read(
        p: &Path,
        home: &Path,
        seen: &mut BTreeSet<PathBuf>,
        names: &mut BTreeSet<String>,
    ) -> Result<()> {
        if !p.is_file() {
            return Ok(());
        }
        let p = fs::canonicalize(p)?;
        if !seen.insert(p.clone()) {
            return Ok(());
        }
        for line in fs::read_to_string(&p)?.lines() {
            let tokens = ssh_words(line);
            let mut words = tokens.iter().map(String::as_str);
            let key = words.next().unwrap_or("").to_lowercase();
            if key == "host" {
                for n in words {
                    if !n.chars().any(|c| "*?![".contains(c)) && !n.starts_with('-') {
                        names.insert(n.into());
                    }
                }
            } else if key == "include" {
                for pattern in words {
                    let pattern = pattern.trim_matches('"');
                    let path = if let Some(s) = pattern.strip_prefix("~/") {
                        home.join(s)
                    } else if Path::new(pattern).is_absolute() {
                        PathBuf::from(pattern)
                    } else if p.starts_with("/etc/ssh") {
                        Path::new("/etc/ssh").join(pattern)
                    } else {
                        home.join(".ssh").join(pattern)
                    };
                    for entry in glob::glob(&path.to_string_lossy())? {
                        read(&entry?, home, seen, names)?;
                    }
                }
            }
        }
        Ok(())
    }
    let mut names = BTreeSet::new();
    let mut seen = BTreeSet::new();
    read(&home.join(".ssh/config"), home, &mut seen, &mut names)?;
    read(
        Path::new("/etc/ssh/ssh_config"),
        home,
        &mut seen,
        &mut names,
    )?;
    Ok(names.into_iter().collect())
}
fn log_files(p: &Path, results: &mut Vec<PathBuf>) -> Result<()> {
    if !p.is_dir() {
        return Ok(());
    }
    for e in fs::read_dir(p)? {
        let e = e?;
        if e.file_type()?.is_dir() {
            log_files(&e.path(), results)?
        } else if e.file_type()?.is_file() {
            let s = e.file_name().to_string_lossy().into_owned();
            if s.ends_with(".log")
                && ["appserver", "app-server", "app_server", "daemon"]
                    .iter()
                    .any(|w| s.contains(w))
            {
                results.push(e.path());
            }
        }
    }
    Ok(())
}
fn logs(app: &App, args: &[String]) -> Result<()> {
    let (mut follow, mut lines, mut file, mut unit) = (false, 100usize, None, None);
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--follow" => follow = true,
            "--file" | "--unit" | "--lines" => {
                let key = args[i].clone();
                i += 1;
                let value = args
                    .get(i)
                    .ok_or_else(|| err("log option requires a value"))?;
                match key.as_str() {
                    "--file" => file = Some(PathBuf::from(value)),
                    "--unit" => unit = Some(value.clone()),
                    _ => lines = value.parse()?,
                }
            }
            _ => return Err(err("unknown logs option")),
        }
        i += 1;
    }
    if !(1..=10000).contains(&lines) {
        return Err(err("--lines must be between 1 and 10000"));
    }
    if let Some(unit) = unit {
        if file.is_some() || unit.is_empty() || unit.starts_with('-') {
            return Err(err("invalid --unit or incompatible --file"));
        }
        let mut c = Command::new("journalctl");
        c.args(["--user", "--no-pager"])
            .arg(format!("--unit={unit}"))
            .args(["-n", &lines.to_string()]);
        if follow {
            c.arg("-f");
        }
        let s = c.status()?;
        if !s.success() {
            return Err(err("journalctl failed"));
        }
        return Ok(());
    }
    let file = match file {
        Some(p) => p,
        None => {
            let mut paths = vec![];
            for e in glob::glob(
                &app.data
                    .join("appserver-ctl-detached.*.log")
                    .to_string_lossy(),
            )? {
                let p = e?;
                if p.is_file() {
                    paths.push(p)
                }
            }
            for p in ["log", "logs", "app-server-control"] {
                log_files(&app.data.join(p), &mut paths)?;
            }
            paths
                .into_iter()
                .max_by_key(|p| fs::metadata(p).and_then(|m| m.modified()).ok())
                .ok_or_else(|| err("no log found; use --file or --unit"))?
        }
    };
    if !file.is_file() {
        return Err(err("log file not found"));
    }
    println!("log={}", file.display());
    let mut c = Command::new("tail");
    c.args(["-n", &lines.to_string()]);
    if follow {
        c.arg("-f");
    }
    c.arg("--").arg(file);
    let s = c.status()?;
    if !s.success() {
        return Err(err("tail failed"));
    }
    Ok(())
}
fn dispatch(app: &mut App, args: &[String]) -> Result<()> {
    let (host, mut args) = target(args)?;
    if args == ["--version"] || args == ["-V"] {
        if let Some(host) = host {
            return remote(&host, &args);
        }
        println!("codex-appserver-ctl {}", env!("CARGO_PKG_VERSION"));
        return Ok(());
    }
    if args.first().is_some_and(|s| s == "save") {
        args.insert(0, "auth".into());
    }
    if let Some(host) = host {
        return remote(&host, &args);
    }
    if args.is_empty() || args.iter().any(|s| matches!(s.as_str(), "-h" | "--help")) {
        if args.first().is_some_and(|s| s == "usage") {
            return usage::report(&app.data, &args[1..]);
        }
        println!("{HELP}");
        return Ok(());
    }
    if args[0] == "usage" {
        return usage::report(&app.data, &args[1..]);
    }
    if args[0] == "limits" {
        return limits::report(app, &args[1..]);
    }
    if args[0] == "logs" {
        return logs(app, &args[1..]);
    }
    let o = options(&args[1..])?;
    if o.internal {
        thread::sleep(Duration::from_secs(1));
    }
    match args[0].as_str() {
        "auth" => {
            let sub = o
                .pos
                .first()
                .ok_or_else(|| err("auth requires list, current, login, save, or use"))?;
            let n = o.pos.get(1).map(String::as_str);
            match sub.as_str() {
                "list" if o.pos.len() == 1 => {
                    for n in app.profiles()? {
                        println!("profile={n}")
                    }
                    println!("{}", app.current()?);
                    Ok(())
                }
                "current" if o.pos.len() == 1 => {
                    println!("{}", app.current()?);
                    Ok(())
                }
                "save" if o.pos.len() == 2 => app.auth_save(n.unwrap(), &o),
                "login" if o.pos.len() == 2 => app.auth_login(n.unwrap(), &o),
                "use" if o.pos.len() <= 2 => app.auth_use(n, &o),
                _ => Err(err(
                    "invalid auth command or arguments; use auth save NAME / auth use [NAME]",
                )),
            }
        }
        "start" | "restart" | "stop" if o.pos.is_empty() => app.control(&args[0], &o),
        "status" if o.pos.is_empty() => app.status(),
        "update" if o.pos.is_empty() => {
            if o.force
                || o.internal
                || args
                    .iter()
                    .any(|s| s.starts_with("--restart") || s == "--no-restart")
            {
                return Err(err("self-update accepts only --timeout N and --dry-run; use update codex for Codex CLI updates"));
            }
            app.update_self(&o)
        }
        "update" if o.pos == ["codex"] => app.update_codex(&o),
        "remote-control" if o.pos.len() == 1 => app.remote_control(&o.pos[0], &o),
        "doctor" if o.pos.is_empty() => app.doctor(),
        "targets" if o.pos.is_empty() => {
            for n in aliases(&app.home)? {
                println!("{n}")
            }
            Ok(())
        }
        "true" | "false" | "1" | "0" if o.pos.is_empty() => app.control(
            if boolean(&args[0])? {
                "restart"
            } else {
                "stop"
            },
            &o,
        ),
        "home" | "this" | "local" | "host" if o.pos.len() == 1 => app.control(
            if boolean(&o.pos[0])? {
                "restart"
            } else {
                "stop"
            },
            &o,
        ),
        x => Err(err(format!(
            "unknown command or invalid arguments: {x}; use --help"
        ))),
    }
}
fn main() {
    let result =
        App::new().and_then(|mut app| dispatch(&mut app, &env::args().skip(1).collect::<Vec<_>>()));
    if let Err(e) = result {
        eprintln!("error: {e}");
        std::process::exit(1);
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    fn app(root: &Path) -> App {
        App {
            home: root.into(),
            data: root.join(".codex"),
            uid: unsafe { libc::getuid() },
            binary: None,
        }
    }
    fn fake_codex(a: &mut App, script: &str) {
        let p = a.home.join("fake-codex");
        fs::write(&p, format!("#!/bin/sh\n{script}\n")).unwrap();
        fs::set_permissions(&p, fs::Permissions::from_mode(0o755)).unwrap();
        a.binary = Some(p);
    }
    #[test]
    fn login_preserves_active_auth_and_failure_leaves_no_profile() {
        let t = tempfile::tempdir().unwrap();
        let mut a = app(t.path());
        fs::create_dir(&a.data).unwrap();
        atomic(&a.data.join("auth.json"), br#"{"active":1}"#).unwrap();
        fake_codex(
            &mut a,
            r#"umask 077; printf '{"new":1}' > "$CODEX_HOME/auth.json""#,
        );
        a.auth_login("new", &options(&[]).unwrap()).unwrap();
        assert!(a.secure(&a.accounts().join("new.json")));
        assert_eq!(
            fs::read(a.data.join("auth.json")).unwrap(),
            br#"{"active":1}"#
        );
        fake_codex(&mut a, "exit 2");
        assert!(a.auth_login("failed", &options(&[]).unwrap()).is_err());
        assert!(!a.accounts().join("failed.json").exists());
    }
    #[test]
    fn restart_failure_rolls_back_auth_and_current_marker() {
        let t = tempfile::tempdir().unwrap();
        let mut a = app(t.path());
        fs::create_dir(&a.data).unwrap();
        a.prepare().unwrap();
        atomic(&a.data.join("auth.json"), br#"{"active":1}"#).unwrap();
        atomic(&a.data.join("current"), b"old\n").unwrap();
        atomic(&a.accounts().join("new.json"), br#"{"new":1}"#).unwrap();
        fake_codex(
            &mut a,
            r#"case "$*" in
            'app-server daemon version') echo '{"status":"running"}' ;;
            'app-server daemon restart') exit 2 ;;
            *) exit 3 ;;
        esac"#,
        );
        let o = options(&["--internal-direct".into()]).unwrap();
        assert!(a.auth_use(Some("new"), &o).is_err());
        assert!(!a.data.join("auth.json").is_symlink());
        assert_eq!(
            fs::read(a.data.join("auth.json")).unwrap(),
            br#"{"active":1}"#
        );
        assert_eq!(fs::read(a.data.join("current")).unwrap(), b"old\n");
    }
    #[test]
    fn selection_without_restart_changes_only_managed_auth() {
        let t = tempfile::tempdir().unwrap();
        let mut a = app(t.path());
        fs::create_dir(&a.data).unwrap();
        a.prepare().unwrap();
        atomic(&a.accounts().join("new.json"), br#"{"new":1}"#).unwrap();
        fake_codex(&mut a, "exit 3");
        let mut o = options(&["--internal-direct".into()]).unwrap();
        // Disable restart in this host-level test to avoid the live desktop process.
        o.restart = false;
        a.auth_use(Some("new"), &o).unwrap();
        assert!(a.data.join("auth.json").is_symlink());
        assert!(a.current().unwrap().contains("current=new"));
    }
    #[test]
    fn profile_validation_and_options() {
        assert!(name("../secret").is_err());
        assert!(name("-bad").is_err());
        assert_eq!(name("work.json").unwrap(), "work");
        assert!(!options(&["--restart=false".into()]).unwrap().restart);
        assert!(options(&["--timeout=0".into()]).is_err());
    }
    #[test]
    fn ssh_config_words_keep_quoted_paths_and_comments() {
        assert_eq!(
            ssh_words("Include=\"hosts with spaces/*\" # ignored"),
            vec!["Include", "hosts with spaces/*"]
        );
        assert_eq!(
            ssh_words("Host MY_SERVER another *.test !excluded"),
            vec!["Host", "MY_SERVER", "another", "*.test", "!excluded"]
        );
    }
    #[test]
    fn ssh_target_and_quoting() {
        assert!(target(&["--target=-oProxyCommand=bad".into()]).is_err());
        assert!(target(&["--target=a".into(), "--target=b".into()]).is_err());
        assert_eq!(
            target(&["save".into(), "x".into(), "--target=MY_SERVER".into()]).unwrap(),
            (Some("MY_SERVER".into()), vec!["save".into(), "x".into()])
        );
        assert_eq!(quote("x'$(touch /tmp/bad)"), "'x'\\''$(touch /tmp/bad)'");
    }
    #[test]
    fn save_atomic_permissions_and_symlink_rejection() {
        let t = tempfile::tempdir().unwrap();
        let a = app(t.path());
        fs::create_dir(&a.data).unwrap();
        atomic(&a.data.join("auth.json"), b"{\"old\":1}").unwrap();
        let o = options(&[]).unwrap();
        a.auth_save("work", &o).unwrap();
        assert!(a.secure(&a.accounts().join("work.json")));
        symlink(
            a.accounts().join("work.json"),
            a.accounts().join("bad.json"),
        )
        .unwrap();
        assert!(a.auth_save("bad", &o).is_err());
    }
    #[test]
    fn snapshot_restores_link_and_bytes() {
        let t = tempfile::tempdir().unwrap();
        let p = t.path().join("auth");
        atomic(&p, b"old").unwrap();
        let s = Snapshot::read(&p).unwrap();
        atomic_link(Path::new("/not/a/real/file"), &p).unwrap();
        s.restore(&p).unwrap();
        assert_eq!(fs::read(&p).unwrap(), b"old");
        let s = Snapshot::read(&p).unwrap();
        atomic(&p, b"new").unwrap();
        s.restore(&p).unwrap();
        assert_eq!(fs::read(p).unwrap(), b"old");
    }
    #[test]
    fn active_profile_cannot_be_overwritten() {
        let t = tempfile::tempdir().unwrap();
        let a = app(t.path());
        fs::create_dir(&a.data).unwrap();
        a.prepare().unwrap();
        let p = a.accounts().join("work.json");
        atomic(&p, b"{\"x\":1}").unwrap();
        atomic_link(&p, &a.data.join("auth.json")).unwrap();
        assert!(a.check_destination(&p, true).is_err());
    }
    #[test]
    fn capture_reports_failure_and_timeout() {
        assert!(
            capture(Command::new("sh").args(["-c", "echo failed; exit 2"]), 2)
                .unwrap_err()
                .to_string()
                .contains("failed")
        );
        assert!(run(Command::new("sleep").arg("2"), 0).is_err());
    }
    #[test]
    fn save_shorthand_and_unknown_commands() {
        let t = tempfile::tempdir().unwrap();
        let mut a = app(t.path());
        fs::create_dir(&a.data).unwrap();
        atomic(&a.data.join("auth.json"), b"{\"x\":1}").unwrap();
        dispatch(&mut a, &["save".into(), "work".into()]).unwrap();
        assert!(a.secure(&a.accounts().join("work.json")));
        assert!(dispatch(&mut a, &["typo".into(), "work".into()])
            .unwrap_err()
            .to_string()
            .contains("unknown command"));
    }
}
