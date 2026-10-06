use crate::{atomic, err, options, App, Result};
use chrono::{Local, TimeZone};
use serde_json::{json, Value};
use std::{
    fs,
    io::{self, IsTerminal, Read, Write},
    os::{fd::AsRawFd, unix::fs::PermissionsExt},
    path::PathBuf,
    process::{Child, Stdio},
    sync::{
        atomic::{AtomicBool, Ordering},
        mpsc,
    },
    thread,
    time::{Duration, Instant},
};

static CANCEL: AtomicBool = AtomicBool::new(false);
extern "C" fn interrupt(_: libc::c_int) {
    CANCEL.store(true, Ordering::Relaxed);
}
struct Signals(libc::sigaction);
impl Drop for Signals {
    fn drop(&mut self) {
        unsafe {
            libc::sigaction(libc::SIGINT, &self.0, std::ptr::null_mut());
        }
    }
}
struct Server(Child);
impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}
#[derive(Clone)]
struct Profile {
    name: String,
    paths: Vec<PathBuf>,
    bytes: Vec<u8>,
    current: bool,
}

fn request(
    server: &mut Server,
    buffer: &mut Vec<u8>,
    id: u64,
    method: &str,
    params: Value,
    deadline: Instant,
) -> std::result::Result<Value, String> {
    writeln!(
        server.0.stdin.as_mut().unwrap(),
        "{}",
        json!({"id":id,"method":method,"params":params})
    )
    .map_err(|_| "server disconnected".to_string())?;
    loop {
        if CANCEL.load(Ordering::Relaxed) {
            return Err("cancelled".into());
        }
        if Instant::now() >= deadline {
            return Err("timed out".into());
        }
        while let Some(end) = buffer.iter().position(|b| *b == b'\n') {
            let line: Vec<_> = buffer.drain(..=end).collect();
            let value: Value =
                serde_json::from_slice(&line).map_err(|_| "invalid server response".to_string())?;
            if value.get("id").and_then(Value::as_u64) == Some(id) {
                if value.get("error").is_some() {
                    return Err("account request failed".into());
                }
                return value
                    .get("result")
                    .cloned()
                    .ok_or_else(|| "missing result".into());
            }
        }
        let stdout = server.0.stdout.as_mut().unwrap();
        let mut poll = libc::pollfd {
            fd: stdout.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        let ready = unsafe { libc::poll(&mut poll, 1, 100) };
        if ready < 0 {
            if io::Error::last_os_error().kind() == io::ErrorKind::Interrupted {
                continue;
            }
            return Err("server read failed".into());
        }
        if ready == 0 {
            continue;
        }
        let mut chunk = [0; 4096];
        match stdout.read(&mut chunk) {
            Ok(0) => return Err("server disconnected".into()),
            Ok(n) => buffer.extend_from_slice(&chunk[..n]),
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(_) => return Err("server read failed".into()),
        }
        if buffer.len() > 1024 * 1024 {
            return Err("server response too large".into());
        }
    }
}
fn check(app: &App, profile: &Profile, timeout: u64) -> std::result::Result<Value, String> {
    let deadline = Instant::now() + Duration::from_secs(timeout);
    let temp = tempfile::tempdir().map_err(|_| "temporary directory failed".to_string())?;
    let auth = temp.path().join("auth.json");
    fs::write(&auth, &profile.bytes).map_err(|_| "auth copy failed".to_string())?;
    fs::set_permissions(&auth, fs::Permissions::from_mode(0o600))
        .map_err(|_| "auth permissions failed".to_string())?;
    let mut command = app
        .command(&[
            "-c",
            "cli_auth_credentials_store=\"file\"",
            "-c",
            "model_provider=\"openai\"",
            "-c",
            "analytics.enabled=false",
            "app-server",
            "--listen",
            "stdio://",
        ])
        .map_err(|_| "Codex CLI unavailable".to_string())?;
    let mut server = Server(
        command
            .env("CODEX_HOME", temp.path())
            .env_remove("OPENAI_API_KEY")
            .env_remove("CODEX_API_KEY")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .map_err(|_| "server start failed".to_string())?,
    );
    let mut buffer = Vec::new();
    request(
        &mut server,
        &mut buffer,
        1,
        "initialize",
        json!({"clientInfo":{"name":"codex-appserver-ctl","version":env!("CARGO_PKG_VERSION")}}),
        deadline,
    )?;
    writeln!(
        server.0.stdin.as_mut().unwrap(),
        "{}",
        json!({"method":"initialized"})
    )
    .map_err(|_| "server disconnected".to_string())?;
    request(
        &mut server,
        &mut buffer,
        2,
        "account/read",
        json!({"refreshToken":false}),
        deadline,
    )?;
    let result = request(
        &mut server,
        &mut buffer,
        3,
        "account/rateLimits/read",
        json!({}),
        deadline,
    );
    drop(server);
    // Persist managed token rotation only if the original files still match the snapshot.
    if app.secure(&auth) {
        let refreshed = fs::read(&auth).map_err(|_| "auth read failed".to_string())?;
        if refreshed != profile.bytes && !CANCEL.load(Ordering::Relaxed) {
            let _lock = app
                .lock(
                    deadline
                        .saturating_duration_since(Instant::now())
                        .as_secs()
                        .saturating_add(1),
                )
                .map_err(|_| "token save lock timed out".to_string())?;
            for path in &profile.paths {
                if !app.secure(path) || !fs::read(path).is_ok_and(|b| b == profile.bytes) {
                    return Err("auth changed during check; retry".into());
                }
            }
            for path in &profile.paths {
                atomic(path, &refreshed).map_err(|_| "token save failed".to_string())?;
            }
        }
    }
    result
}
fn clean(s: &str) -> String {
    s.chars().filter(|c| !c.is_control()).take(80).collect()
}
fn rows(value: &Value) -> Vec<[String; 4]> {
    let mut buckets = Vec::new();
    if let Some(map) = value.get("rateLimitsByLimitId").and_then(Value::as_object) {
        for (id, bucket) in map {
            buckets.push((id.as_str(), bucket));
        }
    }
    if buckets.is_empty() {
        if let Some(bucket) = value.get("rateLimits") {
            buckets.push(("codex", bucket));
        }
    }
    let mut rows = Vec::new();
    for (id, bucket) in buckets {
        for window in ["primary", "secondary"] {
            let Some(w) = bucket
                .get(window)
                .filter(|w| w.get("windowDurationMins").and_then(Value::as_u64) == Some(10080))
            else {
                continue;
            };
            let Some(used) = w
                .get("usedPercent")
                .and_then(Value::as_f64)
                .filter(|u| u.is_finite() && *u >= 0.)
            else {
                continue;
            };
            let remaining = (100. - used).clamp(0., 100.);
            let reset = w
                .get("resetsAt")
                .and_then(Value::as_i64)
                .and_then(|s| Local.timestamp_opt(s, 0).single())
                .map(|t| t.format("%Y-%m-%d %H:%M %:z").to_string())
                .unwrap_or_else(|| "N/A".into());
            let filled = (remaining / 10.).round() as usize;
            rows.push([
                clean(id),
                format!("{used:.1}%"),
                format!(
                    "[{}{}] {remaining:.1}%",
                    "#".repeat(filled),
                    "-".repeat(10 - filled)
                ),
                reset,
            ]);
        }
    }
    rows
}
fn report_once(app: &App, args: &[String]) -> Result<()> {
    let o = options(args)?;
    if !o.pos.is_empty() || o.force || o.dry || o.internal || !o.restart {
        return Err(err("limits accepts only --timeout N"));
    }
    CANCEL.store(false, Ordering::Relaxed);
    let mut old: libc::sigaction = unsafe { std::mem::zeroed() };
    let mut action: libc::sigaction = unsafe { std::mem::zeroed() };
    action.sa_sigaction = interrupt as *const () as usize;
    unsafe {
        libc::sigemptyset(&mut action.sa_mask);
        if libc::sigaction(libc::SIGINT, &action, &mut old) != 0 {
            return Err(io::Error::last_os_error().into());
        }
    }
    let _signals = Signals(old);
    let mut profiles: Vec<Profile> = Vec::new();
    {
        let _lock = app.lock(o.timeout)?;
        let active = app.active()?;
        for name in app.profiles()? {
            let path = fs::canonicalize(app.accounts().join(format!("{name}.json")))?;
            profiles.push(Profile {
                name,
                bytes: fs::read(&path)?,
                current: active.as_ref().is_some_and(|p| p == &path),
                paths: vec![path],
            });
        }
        if let Some(active) = active {
            let bytes = fs::read(&active)?;
            let matches: Vec<_> = profiles.iter_mut().filter(|p| p.bytes == bytes).collect();
            if matches.is_empty() {
                profiles.push(Profile {
                    name: "current".into(),
                    paths: vec![active],
                    bytes,
                    current: true,
                });
            } else {
                for p in matches {
                    p.current = true;
                    if !p.paths.contains(&active) {
                        p.paths.push(active.clone());
                    }
                }
            }
        }
    }
    if profiles.is_empty() {
        println!("No connected accounts. Use auth login NAME.");
        return Ok(());
    }
    let mut groups: Vec<Profile> = Vec::new();
    for p in &profiles {
        if let Some(group) = groups.iter_mut().find(|g| g.bytes == p.bytes) {
            for path in &p.paths {
                if !group.paths.contains(path) {
                    group.paths.push(path.clone());
                }
            }
        } else {
            groups.push(p.clone());
        }
    }
    let start = Instant::now();
    let tty = io::stderr().is_terminal();
    let (tx, rx) = mpsc::channel();
    let mut results = vec![None; groups.len()];
    thread::scope(|scope| {
        for (i, profile) in groups.iter().enumerate() {
            let tx = tx.clone();
            scope.spawn(move || {
                let _ = tx.send((i, check(app, profile, o.timeout)));
            });
        }
        drop(tx);
        let mut done = 0;
        let mut frame = 0;
        while done < groups.len() {
            if tty {
                eprint!(
                    "\r\x1b[2K {} Checking weekly limits {}/{} | {:.1}s elapsed",
                    ["|", "/", "-", "\\"][frame % 4],
                    done,
                    groups.len(),
                    start.elapsed().as_secs_f64()
                );
                let _ = io::stderr().flush();
                frame += 1;
            }
            match rx.recv_timeout(Duration::from_millis(100)) {
                Ok((i, result)) => {
                    results[i] = Some(result);
                    done += 1;
                }
                Err(mpsc::RecvTimeoutError::Timeout) => {}
                Err(_) => break,
            }
        }
    });
    if tty {
        eprint!("\r\x1b[2K");
    }
    if CANCEL.load(Ordering::Relaxed) {
        std::process::exit(130);
    }
    let mut table = vec![[
        "ACCOUNT".into(),
        "LIMIT".into(),
        "USED".into(),
        "REMAINING".into(),
        "RESET (LOCAL)".into(),
    ]];
    let mut failed = false;
    for p in profiles {
        let i = groups.iter().position(|g| g.bytes == p.bytes).unwrap();
        let name = format!("{}{}", if p.current { "* " } else { "  " }, p.name);
        match results[i].as_ref() {
            Some(Ok(value)) => {
                let rows = rows(value);
                if rows.is_empty() {
                    table.push([
                        name,
                        "N/A".into(),
                        "-".into(),
                        "-".into(),
                        "No weekly window".into(),
                    ]);
                } else {
                    for r in rows {
                        table.push([
                            name.clone(),
                            r[0].clone(),
                            r[1].clone(),
                            r[2].clone(),
                            r[3].clone(),
                        ]);
                    }
                }
            }
            other => {
                failed = true;
                let status = other
                    .and_then(|r| r.as_ref().err())
                    .map(String::as_str)
                    .unwrap_or("worker failed");
                table.push([name, "ERROR".into(), "-".into(), "-".into(), status.into()]);
            }
        }
    }
    let widths: Vec<_> = (0..5)
        .map(|i| table.iter().map(|r| r[i].chars().count()).max().unwrap())
        .collect();
    println!(
        "Weekly limits | * current account | checked in {:.1}s",
        start.elapsed().as_secs_f64()
    );
    for (n, row) in table.iter().enumerate() {
        for i in 0..5 {
            print!(
                "{:<width$}{}",
                row[i],
                if i == 4 { "" } else { "  " },
                width = widths[i]
            );
        }
        println!();
        if n == 0 {
            println!("{}", "-".repeat(widths.iter().sum::<usize>() + 8));
        }
    }
    if failed {
        return Err(err("some account checks failed"));
    }
    Ok(())
}
struct Terminal(libc::termios);
impl Drop for Terminal {
    fn drop(&mut self) {
        unsafe {
            libc::tcsetattr(libc::STDIN_FILENO, libc::TCSANOW, &self.0);
        }
    }
}
pub fn report(app: &App, args: &[String]) -> Result<()> {
    let watch = args.iter().any(|s| s == "--watch");
    let args: Vec<_> = args.iter().filter(|s| *s != "--watch").cloned().collect();
    let mut i = 0;
    while i < args.len() {
        if args[i] == "--timeout" {
            i += 2;
        } else if args[i].starts_with("--timeout=") {
            i += 1;
        } else {
            return Err(err("limits accepts --timeout N and --watch"));
        }
    }
    let interactive = watch && io::stdin().is_terminal() && io::stdout().is_terminal();
    loop {
        if interactive {
            print!("\x1b[2J\x1b[H\x1b[1;36mCODEX / WEEKLY LIMITS\x1b[0m\n\n");
        }
        let result = report_once(app, &args);
        if !interactive {
            return result;
        }
        if let Err(e) = &result {
            eprintln!("error: {e}");
        }
        println!("\n[r] refresh   [q] quit");
        io::stdout().flush()?;
        let mut original: libc::termios = unsafe { std::mem::zeroed() };
        if unsafe { libc::tcgetattr(libc::STDIN_FILENO, &mut original) } != 0 {
            return Err(io::Error::last_os_error().into());
        }
        let mut raw = original;
        raw.c_lflag &= !(libc::ICANON | libc::ECHO | libc::ISIG);
        if unsafe { libc::tcsetattr(libc::STDIN_FILENO, libc::TCSANOW, &raw) } != 0 {
            return Err(io::Error::last_os_error().into());
        }
        let terminal = Terminal(original);
        let mut key = [0];
        loop {
            if io::stdin().read(&mut key)? == 0 || key[0] == b'q' {
                return result;
            }
            if key[0] == 3 {
                drop(terminal);
                std::process::exit(130);
            }
            if key[0] == b'r' {
                break;
            }
        }
        drop(terminal);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn weekly_windows_only_and_percent_clamping() {
        let v = json!({"rateLimits":{"primary":{"windowDurationMins":300,"usedPercent":1},"secondary":{"windowDurationMins":10080,"usedPercent":125,"resetsAt":0}}});
        let r = rows(&v);
        assert_eq!(r.len(), 1);
        assert_eq!(r[0][1], "125.0%");
        assert!(r[0][2].ends_with("0.0%"));
        assert!(rows(&json!({"rateLimits":{}})).is_empty());
    }
}
