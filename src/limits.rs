use crate::{atomic, err, options, App, Result};
use chrono::{Local, TimeZone};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::{
    collections::HashMap,
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
#[derive(Clone, Serialize, Deserialize)]
struct Window {
    id: String,
    used: f64,
    reset: Option<i64>,
}
#[derive(Clone, Serialize, Deserialize)]
struct Cached {
    updated_at: i64,
    windows: Vec<Window>,
}
type Cache = HashMap<String, Cached>;
fn rows(value: &Value) -> Vec<Window> {
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
            rows.push(Window {
                id: clean(id),
                used,
                reset: w.get("resetsAt").and_then(Value::as_i64),
            });
        }
    }
    rows
}
fn load_cache(app: &App) -> Cache {
    let path = app.data.join("appserver-ctl-limits.json");
    if !app.secure(&path) {
        return Cache::new();
    }
    fs::read(path)
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok())
        .unwrap_or_default()
}
fn percentage(n: f64) -> String {
    if n.fract() == 0. {
        format!("{n:.0}%")
    } else {
        format!("{n:.1}%")
    }
}
fn reset_at(reset: Option<i64>) -> String {
    let Some(timestamp) = reset else {
        return "N/A".into();
    };
    let mut local: libc::tm = unsafe { std::mem::zeroed() };
    let mut text = [0u8; 64];
    unsafe {
        if libc::localtime_r(&timestamp, &mut local).is_null() {
            return "N/A".into();
        }
        let size = libc::strftime(
            text.as_mut_ptr().cast(),
            text.len(),
            c"%Y-%m-%d %H:%M %Z".as_ptr(),
            &local,
        );
        clean(&String::from_utf8_lossy(&text[..size]))
    }
}
fn reset_in(reset: Option<i64>, now: i64) -> String {
    match reset {
        Some(t) if t <= now => "due".into(),
        Some(t) => {
            let minutes = t.saturating_sub(now) / 60;
            format!(
                "{}d {}h {}m",
                minutes / 1440,
                minutes / 60 % 24,
                minutes % 60
            )
        }
        None => "N/A".into(),
    }
}
fn table(
    profiles: &[Profile],
    cache: &Cache,
    statuses: &HashMap<String, String>,
    refreshing: bool,
    unicode: bool,
) -> String {
    let now = Local::now().timestamp();
    let mut table: Vec<Vec<String>> = vec![[
        "PROFILE",
        "CURRENT",
        "LIMIT",
        "WEEKLY USED",
        "REMAINING",
        "RESETS AT",
        "RESET IN",
        "STATUS",
        "LAST UPDATED",
    ]
    .into_iter()
    .map(String::from)
    .collect()];
    for p in profiles {
        let cached = cache.get(&p.name);
        let status = if refreshing {
            "REFRESHING"
        } else {
            statuses.get(&p.name).map(String::as_str).unwrap_or("N/A")
        };
        let updated = cached
            .and_then(|c| Local.timestamp_opt(c.updated_at, 0).single())
            .map(|t| t.format("%m-%d %H:%M:%S").to_string())
            .unwrap_or_else(|| "-".into());
        let windows = cached.map(|c| c.windows.as_slice()).unwrap_or_default();
        if windows.is_empty() {
            table.push(vec![
                p.name.clone(),
                if p.current { "*" } else { "" }.into(),
                "-".into(),
                "-".into(),
                "-".into(),
                "-".into(),
                "-".into(),
                status.into(),
                updated,
            ]);
        } else {
            for w in windows {
                let remaining = (100. - w.used).clamp(0., 100.);
                let filled = (remaining / 10.).round() as usize;
                let gauge = format!(
                    "[{}{}] {}",
                    if unicode { "█" } else { "#" }.repeat(filled),
                    if unicode { "░" } else { "-" }.repeat(10 - filled),
                    percentage(remaining)
                );
                let reset = reset_at(w.reset);
                table.push(vec![
                    p.name.clone(),
                    if p.current { "*" } else { "" }.into(),
                    clean(&w.id),
                    percentage(w.used),
                    gauge,
                    reset,
                    reset_in(w.reset, now),
                    status.into(),
                    updated.clone(),
                ]);
            }
        }
    }
    let widths: Vec<_> = (0..9)
        .map(|i| table.iter().map(|r| r[i].chars().count()).max().unwrap())
        .collect();
    let mut output = String::new();
    let borders = if unicode {
        ["┌", "┬", "┐", "├", "┼", "┤", "└", "┴", "┘", "─", "│"]
    } else {
        ["+", "+", "+", "+", "+", "+", "+", "+", "+", "-", "|"]
    };
    let border = |left: &str, join: &str, right: &str| {
        format!(
            "{}{}{}\n",
            left,
            widths
                .iter()
                .map(|w| borders[9].repeat(w + 2))
                .collect::<Vec<_>>()
                .join(join),
            right
        )
    };
    output.push_str(&border(borders[0], borders[1], borders[2]));
    for (n, row) in table.iter().enumerate() {
        output.push_str(borders[10]);
        for (i, cell) in row.iter().enumerate() {
            output.push_str(&format!(
                " {cell}{} {}",
                " ".repeat(widths[i].saturating_sub(cell.chars().count())),
                borders[10]
            ));
        }
        output.push('\n');
        if n == 0 {
            output.push_str(&border(borders[3], borders[4], borders[5]));
        }
    }
    output.push_str(&border(borders[6], borders[7], borders[8]));
    output
}
fn terminal_columns() -> usize {
    let mut size: libc::winsize = unsafe { std::mem::zeroed() };
    if unsafe { libc::ioctl(libc::STDOUT_FILENO, libc::TIOCGWINSZ, &mut size) } == 0
        && size.ws_col > 0
    {
        size.ws_col as usize
    } else {
        80
    }
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
    let tty = io::stdout().is_terminal();
    let progress = io::stderr().is_terminal();
    let mut cache = load_cache(app);
    cache.retain(|name, entry| {
        profiles.iter().any(|p| &p.name == name)
            && entry.windows.len() <= 32
            && entry
                .windows
                .iter()
                .all(|w| w.used.is_finite() && w.used >= 0.)
    });
    let mut previous_lines = 0;
    if tty {
        let previous = table(&profiles, &cache, &HashMap::new(), true, true);
        let columns = terminal_columns();
        previous_lines = previous
            .lines()
            .map(|line| line.chars().count().div_ceil(columns).max(1))
            .sum::<usize>();
        print!("{previous}");
        io::stdout().flush()?;
    }
    let (tx, rx) = mpsc::channel();
    let mut results = vec![None; groups.len()];
    let mut updated_at = vec![0; groups.len()];
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
            if progress {
                eprint!(
                    "\r\x1b[2K {} Refreshing weekly limits {}/{} | {:.1}s elapsed",
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
                    updated_at[i] = Local::now().timestamp();
                    done += 1;
                }
                Err(mpsc::RecvTimeoutError::Timeout) => {}
                Err(_) => break,
            }
        }
    });
    if progress {
        eprint!("\r\x1b[2K");
    }
    if CANCEL.load(Ordering::Relaxed) {
        std::process::exit(130);
    }
    let mut statuses = HashMap::new();
    let mut failed = false;
    for p in &profiles {
        let i = groups.iter().position(|g| g.bytes == p.bytes).unwrap();
        match results[i].as_ref() {
            Some(Ok(value)) => {
                let windows = rows(value);
                statuses.insert(
                    p.name.clone(),
                    if windows.is_empty() { "N/A" } else { "OK" }.into(),
                );
                cache.insert(
                    p.name.clone(),
                    Cached {
                        updated_at: updated_at[i],
                        windows,
                    },
                );
            }
            other => {
                failed = true;
                let status = other
                    .and_then(|r| r.as_ref().err())
                    .map(String::as_str)
                    .unwrap_or("worker failed");
                statuses.insert(
                    p.name.clone(),
                    format!(
                        "{}: {status}",
                        if cache.contains_key(&p.name) {
                            "STALE"
                        } else {
                            "ERROR"
                        }
                    ),
                );
            }
        }
    }
    // Cache contains only quota metrics and timestamps, never authentication data.
    if let Err(e) = atomic(
        &app.data.join("appserver-ctl-limits.json"),
        &serde_json::to_vec(&cache)?,
    ) {
        eprintln!("warning: limits cache could not be saved: {e}");
    }
    if tty {
        print!("\x1b[{previous_lines}A\r\x1b[J");
    }
    print!("{}", table(&profiles, &cache, &statuses, false, tty));
    println!(
        "{} | checked in {:.1}s | timestamps are local",
        if failed {
            "Refresh completed with errors; STALE retains previous metrics"
        } else {
            "Refresh complete"
        },
        start.elapsed().as_secs_f64()
    );
    io::stdout().flush()?;
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
    fn cached_table_has_gauge_status_and_account_timestamp() {
        let profile = Profile {
            name: "alpha".into(),
            paths: vec![],
            bytes: vec![],
            current: true,
        };
        let cache = Cache::from([(
            "alpha".into(),
            Cached {
                updated_at: 1800000000,
                windows: vec![Window {
                    id: "codex".into(),
                    used: 9.,
                    reset: Some(1800010000),
                }],
            },
        )]);
        let refreshing = table(
            std::slice::from_ref(&profile),
            &cache,
            &HashMap::new(),
            true,
            true,
        );
        assert!(
            refreshing.contains("┌")
                && refreshing.contains("REFRESHING")
                && refreshing.contains("9%")
                && refreshing.contains("[█████████░] 91%")
                && refreshing.contains("LAST UPDATED")
        );
        let completed = table(
            &[profile],
            &cache,
            &HashMap::from([("alpha".into(), "OK".into())]),
            false,
            false,
        );
        assert!(completed.contains("OK") && !completed.contains("REFRESHING"));
        assert_eq!(
            reset_in(
                Some(1800000000 + 3 * 86400 + 14 * 3600 + 46 * 60),
                1800000000
            ),
            "3d 14h 46m"
        );
        assert_eq!(reset_in(Some(0), 1), "due");
        assert_eq!(reset_in(None, 1), "N/A");
    }
    #[test]
    fn weekly_windows_only_and_percent_clamping() {
        let v = json!({"rateLimits":{"primary":{"windowDurationMins":300,"usedPercent":1},"secondary":{"windowDurationMins":10080,"usedPercent":125,"resetsAt":0}}});
        let r = rows(&v);
        assert_eq!(r.len(), 1);
        assert_eq!(r[0].used, 125.);
        assert_eq!(percentage((100. - r[0].used).clamp(0., 100.)), "0%");
        assert!(rows(&json!({"rateLimits":{}})).is_empty());
    }
}
