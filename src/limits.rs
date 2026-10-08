use crate::{atomic, err, options, App, Options, Result};
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine as _};
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
#[derive(Clone, PartialEq, Eq)]
struct Profile {
    name: String,
    key: String,
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
    if CANCEL.load(Ordering::Relaxed) {
        return Err("cancelled".into());
    }
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
    fs::set_permissions(temp.path(), fs::Permissions::from_mode(0o700))
        .map_err(|_| "temporary directory permissions failed".to_string())?;
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
    let result = (|| {
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
        request(
            &mut server,
            &mut buffer,
            3,
            "account/rateLimits/read",
            json!({}),
            deadline,
        )
    })();
    drop(server);
    // Persist managed token rotation only if the original files still match the snapshot.
    if !app.secure(&auth) {
        let recovery = temp.keep().join("auth.json");
        return Err(format!(
            "auth became unreadable or unsafe; auth preserved at {}",
            recovery.display()
        ));
    }
    let refreshed = match fs::read(&auth) {
        Ok(bytes) => bytes,
        Err(_) => {
            let recovery = temp.keep().join("auth.json");
            return Err(format!(
                "auth read failed; auth preserved at {}",
                recovery.display()
            ));
        }
    };
    if refreshed != profile.bytes {
        let saved = (|| -> Result<()> {
            if identity(&profile.bytes).is_some()
                && identity(&profile.bytes) != identity(&refreshed)
            {
                return Err(err("refreshed auth belongs to a different account"));
            }
            let _lock = app
                .lock_with_cancel(
                    deadline
                        .saturating_duration_since(Instant::now())
                        .as_secs()
                        .saturating_add(1),
                    || CANCEL.load(Ordering::Relaxed),
                )
                .or_else(|e| {
                    if CANCEL.load(Ordering::Relaxed) {
                        app.lock(0)
                    } else {
                        Err(e)
                    }
                })?;
            for path in &profile.paths {
                if !app.secure(path) || !fs::read(path).is_ok_and(|b| b == profile.bytes) {
                    return Err(err("auth changed during check; retry"));
                }
            }
            for path in &profile.paths {
                atomic(path, &refreshed)?;
            }
            Ok(())
        })();
        if let Err(e) = saved {
            let recovery = temp.keep().join("auth.json");
            return Err(format!(
                "token save failed ({e}); refreshed auth preserved at {}",
                recovery.display()
            ));
        }
    }
    result
}
fn clean(s: &str) -> String {
    s.chars()
        .filter(|c| !c.is_control())
        .take(80)
        .map(|c| if c.is_ascii() { c } else { '?' })
        .collect()
}
#[derive(Clone, Serialize, Deserialize)]
struct Window {
    id: String,
    used: f64,
    reset: Option<i64>,
}
#[derive(Clone, Serialize, Deserialize, PartialEq, Eq)]
struct Identity {
    user: String,
    account: String,
}
fn identity(bytes: &[u8]) -> Option<Identity> {
    let value: Value = serde_json::from_slice(bytes).ok()?;
    let tokens = value.get("tokens")?;
    let jwt = tokens.get("id_token")?.as_str()?;
    let parts: Vec<_> = jwt.split('.').collect();
    if parts.len() != 3 || parts.iter().any(|p| p.is_empty()) {
        return None;
    }
    let payload = URL_SAFE_NO_PAD.decode(parts[1]).ok()?;
    let claims: Value = serde_json::from_slice(&payload).ok()?;
    let auth = claims.get("https://api.openai.com/auth")?;
    let user = auth
        .get("chatgpt_user_id")
        .and_then(Value::as_str)
        .or_else(|| auth.get("user_id").and_then(Value::as_str))
        .or_else(|| claims.get("sub").and_then(Value::as_str))?;
    let account = tokens
        .get("account_id")
        .and_then(Value::as_str)
        .or_else(|| auth.get("chatgpt_account_id").and_then(Value::as_str))?;
    if user.is_empty() || account.is_empty() || user.len() > 256 || account.len() > 256 {
        return None;
    }
    Some(Identity {
        user: user.into(),
        account: account.into(),
    })
}
fn same_account(app: &App, profile: &Profile) -> bool {
    let expected = identity(&profile.bytes);
    profile.paths.iter().all(|path| {
        app.secure(path)
            && fs::read(path).is_ok_and(|bytes| {
                if expected.is_some() {
                    identity(&bytes) == expected
                } else {
                    bytes == profile.bytes
                }
            })
    })
}
#[derive(Clone, Serialize, Deserialize)]
struct Cached {
    #[serde(default)]
    identity: Option<Identity>,
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
fn load_cache(app: &App, profiles: &[Profile]) -> Cache {
    let path = app.data.join("appserver-ctl-limits.json");
    if !app.secure(&path) {
        return Cache::new();
    }
    let mut cache: Cache = fs::read(path)
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok())
        .unwrap_or_default();
    cache.retain(|name, entry| {
        profiles.iter().any(|p| {
            &p.key == name && entry.identity.is_some() && entry.identity == identity(&p.bytes)
        }) && entry.windows.len() <= 32
            && entry
                .windows
                .iter()
                .all(|w| w.used.is_finite() && w.used >= 0.)
    });
    cache
}
fn percentage(n: f64) -> String {
    if n >= 10000. {
        format!("{n:.1e}%")
    } else if n.fract() == 0. {
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
    columns: usize,
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
        let cached = cache.get(&p.key);
        let status = if refreshing {
            "REFRESHING"
        } else {
            statuses.get(&p.key).map(String::as_str).unwrap_or("N/A")
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
    let full_table = table;
    let widths_for = |rows: &[Vec<String>]| -> Vec<usize> {
        (0..rows[0].len())
            .map(|i| rows.iter().map(|r| r[i].chars().count()).max().unwrap())
            .collect()
    };
    let full_width = widths_for(&full_table).iter().sum::<usize>() + 3 * 9 + 1;
    let compact = full_width > columns;
    let mut compact_table = Vec::new();
    if compact {
        let selection = if columns >= 75 {
            vec![0, 1, 3, 4, 7]
        } else if columns >= 60 {
            vec![0, 3, 4, 7]
        } else {
            vec![0, 3]
        };
        compact_table = full_table
            .iter()
            .map(|r| selection.iter().map(|i| r[*i].clone()).collect())
            .collect();
        // All omitted columns remain available below, including full profile names.
        let widths = widths_for(&compact_table);
        let other = widths.iter().skip(1).sum::<usize>() + 3 * widths.len() + 1;
        let limit = columns.saturating_sub(other).max(7);
        for r in &mut compact_table {
            if r[0].chars().count() > limit {
                r[0] = format!("{}…", r[0].chars().take(limit - 1).collect::<String>());
            }
        }
    }
    let table = if compact { &compact_table } else { &full_table };
    let widths = widths_for(table);
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
    if compact {
        for row in full_table.iter().skip(1) {
            for line in [
                format!(
                    "{}{} / {}: {} remaining, {}",
                    if row[1] == "*" { "* " } else { "" },
                    row[0],
                    row[2],
                    row[4],
                    row[7]
                ),
                format!("Reset: {} ({}) | Last updated: {}", row[5], row[6], row[8]),
            ] {
                let chars: Vec<_> = line.chars().collect();
                for chunk in chars.chunks(columns.max(1)) {
                    output.extend(chunk);
                    output.push('\n');
                }
            }
        }
    }
    output
}
fn terminal_size() -> (usize, usize) {
    let mut size: libc::winsize = unsafe { std::mem::zeroed() };
    let fd = if io::stdout().is_terminal() {
        libc::STDOUT_FILENO
    } else {
        libc::STDERR_FILENO
    };
    if unsafe { libc::ioctl(fd, libc::TIOCGWINSZ, &mut size) } == 0
        && size.ws_col > 0
        && size.ws_row > 0
    {
        (size.ws_col as usize, size.ws_row as usize)
    } else {
        (80, 24)
    }
}
fn redraw(lines: usize, size: (usize, usize)) {
    // Only replace a frame still fully visible at the same terminal dimensions.
    if lines > 0 && lines + 1 < size.1 && size == terminal_size() {
        print!("\x1b[{lines}A\r\x1b[J");
    } else {
        println!();
    }
}
fn print_progress(text: &str) {
    let tty = io::stdout().is_terminal();
    if !tty && !io::stderr().is_terminal() {
        return;
    }
    let text: String = text
        .chars()
        .take(terminal_size().0.saturating_sub(1))
        .collect();
    if tty {
        print!("\r\x1b[2K{text}");
        let _ = io::stdout().flush();
    } else {
        eprint!("\r\x1b[2K{text}");
        let _ = io::stderr().flush();
    }
}
fn wait_for_lock(app: &App, filename: &str, label: &str, timeout: u64) -> Result<fs::File> {
    let waiting = Instant::now();
    let lock = app.lock_named(filename, timeout, || {
        if waiting.elapsed() > Duration::from_millis(250) {
            print_progress(&format!(
                " {} Waiting for {label} | {:.1}s | Ctrl-C cancels",
                ["|", "/", "-", "\\"][(waiting.elapsed().as_millis() / 100 % 4) as usize],
                waiting.elapsed().as_secs_f64()
            ));
        }
        CANCEL.load(Ordering::Relaxed)
    });
    if waiting.elapsed() > Duration::from_millis(250) {
        print_progress("");
    }
    match lock {
        Err(_) if CANCEL.load(Ordering::Relaxed) => {
            eprintln!("Cancelled while waiting for {label}.");
            std::process::exit(130);
        }
        other => other,
    }
}
fn read_profiles(app: &App, timeout: u64) -> Result<Vec<Profile>> {
    let _lock = wait_for_lock(app, "appserver-ctl-auth.lock", "auth lock", timeout)?;
    let mut profiles = Vec::new();
    let active = app.active()?;
    for name in app.profiles()? {
        let path = fs::canonicalize(app.accounts().join(format!("{name}.json")))?;
        profiles.push(Profile {
            key: format!("profile:{name}"),
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
                name: "(active)".into(),
                key: "active".into(),
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
    Ok(profiles)
}
fn mark_current(app: &App, profiles: &mut [Profile]) -> Result<()> {
    for profile in profiles.iter_mut() {
        profile.current = false;
    }
    if let Some(active) = app.active()? {
        let bytes = fs::read(&active)?;
        for profile in profiles {
            let source = &profile.paths[0];
            profile.current = source == &active
                || (app.secure(source) && fs::read(source).is_ok_and(|b| b == bytes));
        }
    }
    Ok(())
}
fn show_pending(profiles: &[Profile], cache: &Cache) -> Result<(usize, (usize, usize))> {
    let size = terminal_size();
    if !io::stdout().is_terminal() {
        return Ok((0, size));
    }
    let text = table(profiles, cache, &HashMap::new(), true, true, size.0);
    let lines = text
        .lines()
        .map(|line| line.chars().count().div_ceil(size.0).max(1))
        .sum();
    print!("{text}");
    io::stdout().flush()?;
    Ok((lines, size))
}
fn report_once(app: &App, o: &Options) -> Result<()> {
    let mut profiles = read_profiles(app, o.timeout)?;
    if profiles.is_empty() {
        println!("No connected accounts. Use auth login NAME.");
        return Ok(());
    }
    let start = Instant::now();
    let tty = io::stdout().is_terminal();
    let progress = tty || io::stderr().is_terminal();
    let mut cache = load_cache(app, &profiles);
    let (mut previous_lines, mut size) = show_pending(&profiles, &cache)?;
    // ponytail: one batch per account store; per-account locks if independent batch throughput matters.
    let _refresh = wait_for_lock(
        app,
        "appserver-ctl-limits.lock",
        "another limits refresh",
        o.timeout,
    )?;
    let fresh = read_profiles(app, o.timeout)?;
    let queued = start.elapsed() > Duration::from_millis(250) || fresh != profiles;
    profiles = fresh;
    cache = load_cache(app, &profiles);
    if profiles.is_empty() {
        if tty {
            redraw(previous_lines, size);
        }
        println!("No connected accounts. Use auth login NAME.");
        return Ok(());
    }
    if queued {
        if tty {
            redraw(previous_lines, size);
        }
        (previous_lines, size) = show_pending(&profiles, &cache)?;
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
                let text = format!(
                    " {} Refreshing weekly limits {}/{} | {:.1}s elapsed",
                    ["|", "/", "-", "\\"][frame % 4],
                    done,
                    groups.len(),
                    start.elapsed().as_secs_f64()
                );
                print_progress(&text);
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
        print_progress("");
    }
    let active_error = mark_current(app, &mut profiles)
        .err()
        .map(|e| format!("active account check failed: {e}"));
    // A concurrent account replacement invalidates old metrics on cancellation too.
    cache.retain(|key, _| {
        profiles
            .iter()
            .any(|p| &p.key == key && same_account(app, p))
    });
    if CANCEL.load(Ordering::Relaxed) {
        if tty {
            redraw(previous_lines, size);
            let statuses = profiles
                .iter()
                .map(|p| (p.key.clone(), "CANCELLED".into()))
                .collect();
            print!(
                "{}",
                table(&profiles, &cache, &statuses, false, true, terminal_size().0)
            );
            let _ = io::stdout().flush();
        }
        eprintln!("\nCancelled; previous limits remain unchanged.");
        for (i, result) in results.iter().enumerate() {
            if let Some(Err(e)) = result {
                if e.contains("preserved at") {
                    eprintln!("{}: {e}", groups[i].name);
                }
            }
        }
        if let Some(error) = active_error {
            eprintln!("{error}");
        }
        std::process::exit(130);
    }
    let mut statuses = HashMap::new();
    let mut failed = active_error.is_some();
    let mut errors: Vec<String> = active_error.into_iter().collect();
    for p in &profiles {
        let i = groups.iter().position(|g| g.bytes == p.bytes).unwrap();
        let result = results[i].as_ref();
        let changed = !same_account(app, p);
        if changed {
            cache.remove(&p.key);
        }
        let changed_result = Err("account changed during check; retry".to_string());
        let result = if changed {
            Some(&changed_result)
        } else {
            result
        };
        match result {
            Some(Ok(value)) => {
                let windows = rows(value);
                statuses.insert(
                    p.key.clone(),
                    if windows.is_empty() { "N/A" } else { "OK" }.into(),
                );
                cache.insert(
                    p.key.clone(),
                    Cached {
                        identity: identity(&p.bytes),
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
                errors.push(format!("{}: {status}", p.name));
                if changed {
                    if let Some(Err(e)) = results[i].as_ref() {
                        if e.contains("preserved at") {
                            errors.push(e.clone());
                        }
                    }
                }
                statuses.insert(
                    p.key.clone(),
                    if cache.contains_key(&p.key) {
                        "STALE"
                    } else {
                        "ERROR"
                    }
                    .into(),
                );
            }
        }
    }
    // Cache contains metrics, timestamps and stable identity, never tokens.
    let saved_cache: HashMap<_, _> = cache
        .iter()
        .filter(|(_, entry)| entry.identity.is_some())
        .collect();
    let cache_save = atomic(
        &app.data.join("appserver-ctl-limits.json"),
        &serde_json::to_vec(&saved_cache)?,
    );
    drop(_refresh);
    if tty {
        redraw(previous_lines, size);
    }
    print!(
        "{}",
        table(
            &profiles,
            &cache,
            &statuses,
            false,
            tty,
            if tty { terminal_size().0 } else { usize::MAX }
        )
    );
    for error in errors {
        eprintln!("{error}");
    }
    if let Err(e) = cache_save {
        eprintln!("warning: limits cache could not be saved: {e}");
    }
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
    let o = options(&args)?;
    if !o.pos.is_empty() || o.force || o.dry || o.internal || !o.restart {
        return Err(err("limits accepts --timeout N and --watch"));
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
    let interactive = watch && io::stdin().is_terminal() && io::stdout().is_terminal();
    loop {
        if interactive {
            print!("\x1b[2J\x1b[H\x1b[1;36mCODEX / WEEKLY LIMITS\x1b[0m\n\n");
        }
        let result = report_once(app, &o);
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
            if CANCEL.load(Ordering::Relaxed) {
                drop(terminal);
                std::process::exit(130);
            }
            let mut poll = libc::pollfd {
                fd: libc::STDIN_FILENO,
                events: libc::POLLIN,
                revents: 0,
            };
            let ready = unsafe { libc::poll(&mut poll, 1, 100) };
            if ready < 0 {
                let error = io::Error::last_os_error();
                if error.kind() == io::ErrorKind::Interrupted {
                    continue;
                }
                return Err(error.into());
            }
            if ready == 0 {
                continue;
            }
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
            key: "profile:alpha".into(),
            paths: vec![],
            bytes: vec![],
            current: true,
        };
        let cache = Cache::from([(
            "profile:alpha".into(),
            Cached {
                identity: None,
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
            usize::MAX,
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
            &HashMap::from([("profile:alpha".into(), "OK".into())]),
            false,
            false,
            usize::MAX,
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
