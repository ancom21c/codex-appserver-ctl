use crate::{err, Result};
use chrono::{DateTime, Local, NaiveDate};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    io::{BufRead, BufReader},
    path::{Path, PathBuf},
};

#[derive(Default, Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Tokens {
    #[serde(default)]
    pub input_tokens: u64,
    #[serde(default)]
    pub cached_input_tokens: u64,
    #[serde(default)]
    pub cache_write_input_tokens: u64,
    #[serde(default)]
    pub output_tokens: u64,
    #[serde(default)]
    pub reasoning_output_tokens: u64,
    #[serde(default)]
    pub total_tokens: u64,
}
impl Tokens {
    fn add(&mut self, b: &Self) {
        self.input_tokens += b.input_tokens;
        self.cached_input_tokens += b.cached_input_tokens;
        self.cache_write_input_tokens += b.cache_write_input_tokens;
        self.output_tokens += b.output_tokens;
        self.reasoning_output_tokens += b.reasoning_output_tokens;
        self.total_tokens += b.total_tokens;
    }
    fn delta(&self, previous: &Self) -> Self {
        Self {
            input_tokens: self.input_tokens.saturating_sub(previous.input_tokens),
            cached_input_tokens: self
                .cached_input_tokens
                .saturating_sub(previous.cached_input_tokens),
            cache_write_input_tokens: self
                .cache_write_input_tokens
                .saturating_sub(previous.cache_write_input_tokens),
            output_tokens: self.output_tokens.saturating_sub(previous.output_tokens),
            reasoning_output_tokens: self
                .reasoning_output_tokens
                .saturating_sub(previous.reasoning_output_tokens),
            total_tokens: self.total_tokens.saturating_sub(previous.total_tokens),
        }
    }
}
#[derive(Deserialize)]
struct Price {
    input: f64,
    cached: f64,
    output: f64,
}
#[derive(Serialize, Default)]
pub struct Row {
    pub period: String,
    pub model: String,
    #[serde(flatten)]
    pub tokens: Tokens,
    pub estimated_cost_usd: Option<f64>,
}
#[derive(Serialize)]
pub struct Report {
    pub source: String,
    pub timezone: String,
    pub files: usize,
    pub malformed_lines: usize,
    pub rows: Vec<Row>,
    pub total: Tokens,
}
fn walk(root: &Path, dir: &Path, paths: &mut BTreeMap<PathBuf, PathBuf>) -> Result<()> {
    if !dir.is_dir() {
        return Ok(());
    }
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        let ty = entry.file_type()?;
        let p = entry.path();
        if ty.is_dir() {
            walk(root, &p, paths)?;
        } else if ty.is_file() && p.extension().is_some_and(|x| x == "jsonl") {
            paths.insert(p.strip_prefix(root)?.into(), p);
        }
    }
    Ok(())
}
fn date(s: &str) -> Result<NaiveDate> {
    Ok(NaiveDate::parse_from_str(s, "%Y-%m-%d")?)
}
fn take(args: &[String], i: &mut usize) -> Result<String> {
    *i += 1;
    args.get(*i)
        .cloned()
        .ok_or_else(|| err("option requires a value"))
}
pub fn report(home: &Path, args: &[String]) -> Result<()> {
    let mut mode = "daily".to_string();
    let mut json = false;
    let mut since = None;
    let mut until = None;
    let mut last = None;
    let mut prices_file = None;
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "daily" | "monthly" | "session" if i == 0 => mode = args[i].clone(),
            "--json" => json = true,
            "--since" => since = Some(date(&take(args, &mut i)?)?),
            "--until" => until = Some(date(&take(args, &mut i)?)?),
            "--last" => {
                let n: u32 = take(args, &mut i)?.parse()?;
                if n == 0 {
                    return Err(err("--last must be positive"));
                }
                last = Some(n);
            }
            "--prices" => prices_file = Some(take(args, &mut i)?),
            "--help" | "-h" => {
                println!("usage [daily|monthly|session] [--since YYYY-MM-DD] [--until YYYY-MM-DD] [--last DAYS] [--json] [--prices FILE]\nDates use the execution host's local timezone. --last always means calendar days.\nUsage is recorded local history, not remaining account quota. Prices are USD per million tokens.");
                return Ok(());
            }
            x => return Err(err(format!("unknown usage option: {x}"))),
        }
        i += 1;
    }
    if let Some(n) = last {
        let start = Local::now().date_naive() - chrono::Duration::days((n - 1) as i64);
        since = Some(since.map_or(start, |s| s.max(start)));
    }
    if since.zip(until).is_some_and(|(s, u)| s > u) {
        return Err(err("--since must not be after --until"));
    }
    let prices: BTreeMap<String, Price> = match prices_file {
        Some(p) => serde_json::from_slice(&fs::read(p)?)?,
        None => BTreeMap::new(),
    };
    if prices.values().any(|p| {
        [p.input, p.cached, p.output]
            .iter()
            .any(|v| !v.is_finite() || *v < 0.0)
    }) {
        return Err(err("prices must be finite non-negative numbers"));
    }
    let result = collect(home, &mode, since, until, &prices)?;
    if json {
        println!("{}", serde_json::to_string_pretty(&result)?);
    } else {
        println!("Local Codex usage ({mode}; host local timezone). Estimated costs are not billed charges.");
        println!(
            "{:<36} {:<24} {:>12} {:>12} {:>12} {:>12} {:>12}",
            "Period/session", "Model", "Input", "Cached", "Output", "Total", "USD estimate"
        );
        for r in &result.rows {
            println!(
                "{:<36} {:<24} {:>12} {:>12} {:>12} {:>12} {:>12}",
                r.period,
                r.model,
                r.tokens.input_tokens,
                r.tokens.cached_input_tokens,
                r.tokens.output_tokens,
                r.tokens.total_tokens,
                r.estimated_cost_usd
                    .map_or("-".into(), |v| format!("{v:.4}"))
            );
        }
        println!(
            "Total: {} tokens; {} files; {} malformed lines skipped.",
            result.total.total_tokens, result.files, result.malformed_lines
        );
    }
    Ok(())
}
fn collect(
    home: &Path,
    mode: &str,
    since: Option<NaiveDate>,
    until: Option<NaiveDate>,
    prices: &BTreeMap<String, Price>,
) -> Result<Report> {
    let mut files = BTreeMap::new();
    walk(
        &home.join("archived_sessions"),
        &home.join("archived_sessions"),
        &mut files,
    )?;
    walk(&home.join("sessions"), &home.join("sessions"), &mut files)?; // Active copy wins.
    let mut totals: BTreeMap<String, Tokens> = BTreeMap::new();
    let mut seen = BTreeSet::new();
    let mut rows: BTreeMap<(String, String), Row> = BTreeMap::new();
    let mut malformed = 0;
    for path in files.values() {
        let mut session = path
            .file_stem()
            .unwrap_or_default()
            .to_string_lossy()
            .to_string();
        let mut model = "unknown".to_string();
        for line in BufReader::new(fs::File::open(path)?).lines() {
            let line = line?;
            if line.trim().is_empty() {
                continue;
            }
            let value: Value = match serde_json::from_str(&line) {
                Ok(v) => v,
                Err(_) => {
                    malformed += 1;
                    continue;
                }
            };
            let p = &value["payload"];
            match value["type"].as_str() {
                Some("session_meta") => {
                    if let Some(id) = p["id"].as_str() {
                        session = id.into();
                    }
                }
                Some("turn_context") => {
                    if let Some(m) = p["model"].as_str() {
                        model = m.into();
                    }
                }
                _ => {}
            }
            if value["type"] != "event_msg" || p["type"] != "token_count" {
                continue;
            }
            let ts = match value["timestamp"]
                .as_str()
                .and_then(|s| DateTime::parse_from_rfc3339(s).ok())
            {
                Some(t) => t.with_timezone(&Local),
                None => {
                    malformed += 1;
                    continue;
                }
            };
            let info = &p["info"];
            if info.is_null() {
                continue;
            }
            let identity = format!("{session}|{}|{info}", ts.to_rfc3339());
            if !seen.insert(identity) {
                continue;
            }
            let previous = totals.entry(session.clone()).or_default();
            let delta = if !info["total_token_usage"].is_null() {
                let current: Tokens =
                    match serde_json::from_value(info["total_token_usage"].clone()) {
                        Ok(t) => t,
                        Err(_) => {
                            malformed += 1;
                            continue;
                        }
                    };
                let delta = if current.total_tokens < previous.total_tokens {
                    serde_json::from_value(info["last_token_usage"].clone())
                        .unwrap_or_else(|_| current.clone())
                } else {
                    current.delta(previous)
                };
                *previous = current;
                delta
            } else {
                let increment: Tokens =
                    match serde_json::from_value(info["last_token_usage"].clone()) {
                        Ok(t) => t,
                        Err(_) => {
                            malformed += 1;
                            continue;
                        }
                    };
                previous.add(&increment);
                increment
            };
            let day = ts.date_naive(); // Update baseline before date filtering.
            if since.is_some_and(|s| day < s) || until.is_some_and(|u| day > u) {
                continue;
            }
            let period = match mode {
                "monthly" => ts.format("%Y-%m").to_string(),
                "session" => session.clone(),
                _ => day.to_string(),
            };
            let row = rows
                .entry((period.clone(), model.clone()))
                .or_insert_with(|| Row {
                    period,
                    model: model.clone(),
                    ..Row::default()
                });
            row.tokens.add(&delta);
            if let Some(price) = prices.get(&model) {
                let cost = (delta.input_tokens.saturating_sub(delta.cached_input_tokens) as f64
                    * price.input
                    + delta.cached_input_tokens as f64 * price.cached
                    + delta.output_tokens as f64 * price.output)
                    / 1_000_000.0;
                row.estimated_cost_usd = Some(row.estimated_cost_usd.unwrap_or(0.0) + cost);
            }
        }
    }
    let rows: Vec<Row> = rows
        .into_values()
        .filter(|r| {
            r.tokens.total_tokens > 0 || r.tokens.input_tokens > 0 || r.tokens.output_tokens > 0
        })
        .collect();
    let mut total = Tokens::default();
    for row in &rows {
        total.add(&row.tokens);
    }
    Ok(Report {
        source: home.display().to_string(),
        timezone: "execution host local timezone".into(),
        files: files.len(),
        malformed_lines: malformed,
        rows,
        total,
    })
}
#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    #[test]
    fn deduplicates_cumulative_archives_and_date_filters() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        fs::create_dir_all(root.join("sessions")).unwrap();
        fs::create_dir_all(root.join("archived_sessions")).unwrap();
        let meta = json!({"type":"session_meta","payload":{"id":"s"}});
        let ctx = json!({"type":"turn_context","payload":{"model":"test"}});
        let event = |ts: &str, i, o| json!({"type":"event_msg","timestamp":ts,"payload":{"type":"token_count","info":{"total_token_usage":{"input_tokens":i,"cached_input_tokens":i/2,"output_tokens":o,"reasoning_output_tokens":o/2,"total_tokens":i+o}}}});
        let records = [
            meta,
            ctx,
            event("2026-01-01T12:00:00Z", 100, 20),
            event("2026-01-02T12:00:00Z", 150, 30),
            event("2026-01-02T12:00:01Z", 150, 30),
        ];
        let data = records.iter().map(|r| format!("{r}\n")).collect::<String>();
        fs::write(root.join("sessions/a.jsonl"), &data).unwrap();
        fs::write(root.join("archived_sessions/a.jsonl"), &data).unwrap();
        let mut prices = BTreeMap::new();
        prices.insert(
            "test".into(),
            Price {
                input: 1.0,
                cached: 0.5,
                output: 2.0,
            },
        );
        let all = collect(root, "daily", None, None, &prices).unwrap();
        assert_eq!(all.files, 1);
        assert_eq!(all.total.total_tokens, 180);
        assert_eq!(all.total.output_tokens, 30);
        let filtered = collect(
            root,
            "daily",
            Some(date("2026-01-02").unwrap()),
            None,
            &prices,
        )
        .unwrap();
        assert_eq!(filtered.total.total_tokens, 60);
        assert_eq!(filtered.total.cached_input_tokens, 25);
        assert!(filtered.rows[0].estimated_cost_usd.is_some());
    }
    #[test]
    fn reset_and_last_only_do_not_double_reasoning() {
        let d = tempfile::tempdir().unwrap();
        fs::create_dir(d.path().join("sessions")).unwrap();
        let e = |total, last| json!({"type":"event_msg","timestamp":"2026-02-01T12:00:00Z","payload":{"type":"token_count","info":{"total_token_usage":total,"last_token_usage":last}}});
        let t = |i, o| json!({"input_tokens":i,"output_tokens":o,"reasoning_output_tokens":o,"total_tokens":i+o});
        let events = [
            e(t(100, 20), t(100, 20)),
            e(t(10, 2), t(10, 2)),
            e(Value::Null, t(5, 1)),
            e(t(15, 3), t(5, 1)), // Cumulative snapshot already includes the last-only record.
        ];
        fs::write(
            d.path().join("sessions/x.jsonl"),
            events.iter().map(|v| format!("{v}\n")).collect::<String>(),
        )
        .unwrap();
        let r = collect(d.path(), "session", None, None, &BTreeMap::new()).unwrap();
        assert_eq!(r.total.total_tokens, 138);
        assert_eq!(r.total.output_tokens, 23);
        assert!(r.rows[0].estimated_cost_usd.is_none());
    }
}
