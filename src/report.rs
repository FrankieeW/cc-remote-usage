//! 用量报告：解析本机镜像的会话日志，按服务器 / 模型 / 日期汇总。
//!
//! 花费用 CC Switch 自己的 `model-pricing.json` 计价，口径与它的看板一致。

use crate::{home, root};
use anyhow::Result;
use chrono::{DateTime, Local, NaiveDate};
use std::collections::{BTreeMap, HashMap};
use std::fs;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone)]
struct Usage {
    time: DateTime<Local>,
    model: String,
    input: i64,
    output: i64,
    cache_read: i64,
    cache_creation: i64,
}

#[derive(Default, Clone)]
struct Bucket {
    req: i64,
    input: i64,
    output: i64,
    cache_read: i64,
    cache_creation: i64,
    cost: f64,
    unpriced: bool,
}

impl Bucket {
    fn add(&mut self, u: &Usage, price: Option<(f64, f64, f64, f64)>) {
        self.req += 1;
        self.input += u.input;
        self.output += u.output;
        self.cache_read += u.cache_read;
        self.cache_creation += u.cache_creation;
        match price {
            Some((pi, po, pcr, pcw)) => {
                self.cost += u.input as f64 * pi / 1e6
                    + u.output as f64 * po / 1e6
                    + u.cache_read as f64 * pcr / 1e6
                    + u.cache_creation as f64 * pcw / 1e6;
            }
            None => self.unpriced = true,
        }
    }

    fn merge(&mut self, o: &Bucket) {
        self.req += o.req;
        self.input += o.input;
        self.output += o.output;
        self.cache_read += o.cache_read;
        self.cache_creation += o.cache_creation;
        self.cost += o.cost;
        self.unpriced |= o.unpriced;
    }

    fn cost_str(&self) -> String {
        if self.unpriced && self.cost == 0.0 {
            "n/a".to_string()
        } else if self.unpriced {
            format!("{:.4}+", self.cost)
        } else {
            format!("{:.4}", self.cost)
        }
    }
}

// ---------------------------------------------------------------- 定价

fn load_pricing() -> HashMap<String, (f64, f64, f64, f64)> {
    let path = home().join(".cc-switch").join("model-pricing.json");
    let mut out = HashMap::new();
    let Ok(text) = fs::read_to_string(path) else {
        return out;
    };
    let Ok(v) = serde_json::from_str::<serde_json::Value>(&text) else {
        return out;
    };
    let Some(models) = v["models"].as_array() else {
        return out;
    };
    for m in models {
        let Some(id) = m["modelId"].as_str() else {
            continue;
        };
        let f = |k: &str| {
            m[k]
                .as_str()
                .and_then(|s| s.parse::<f64>().ok())
                .or_else(|| m[k].as_f64())
                .unwrap_or(0.0)
        };
        out.insert(
            id.to_ascii_lowercase(),
            (
                f("inputCostPerMillion"),
                f("outputCostPerMillion"),
                f("cacheReadCostPerMillion"),
                f("cacheCreationCostPerMillion"),
            ),
        );
    }
    out
}

fn price_for<'a>(
    pricing: &'a HashMap<String, (f64, f64, f64, f64)>,
    model: &str,
) -> Option<(f64, f64, f64, f64)> {
    let m = model.trim().to_ascii_lowercase();
    if m.is_empty() {
        return None;
    }
    if let Some(p) = pricing.get(&m) {
        return Some(*p);
    }
    // 前缀匹配（模型名带日期/后缀时），取最长命中
    let mut best: Option<(&String, &(f64, f64, f64, f64))> = None;
    for (k, p) in pricing {
        if k.len() >= 4 && m.starts_with(k) {
            if best.map(|(bk, _)| k.len() > bk.len()).unwrap_or(true) {
                best = Some((k, p));
            }
        }
    }
    best.map(|(_, p)| *p)
}

// ---------------------------------------------------------------- 扫描

fn collect_jsonl(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(rd) = fs::read_dir(dir) else { return };
    for e in rd.flatten() {
        let p = e.path();
        if p.is_dir() {
            collect_jsonl(&p, out);
        } else if p.extension().map(|x| x == "jsonl").unwrap_or(false) {
            out.push(p);
        }
    }
}

fn parse_ts(s: &str) -> Option<DateTime<Local>> {
    DateTime::parse_from_rfc3339(s)
        .ok()
        .map(|d| d.with_timezone(&Local))
}

fn scan_codex(path: &Path, out: &mut Vec<Usage>) {
    let Ok(f) = fs::File::open(path) else { return };
    let mut model: Option<String> = None;
    for line in BufReader::new(f).lines().map_while(Result::ok) {
        if !line.contains("token_usage_record") && !line.contains("turn_context") {
            continue;
        }
        let Ok(v) = serde_json::from_str::<serde_json::Value>(&line) else {
            continue;
        };
        match v["type"].as_str() {
            Some("turn_context") => {
                if let Some(m) = v["payload"]["model"].as_str() {
                    model = Some(m.to_string());
                }
            }
            Some("token_usage_record") => {
                let Some(ts) = v["timestamp"].as_str().and_then(parse_ts) else {
                    continue;
                };
                let u = &v["payload"]["usage"];
                let input = u["input_tokens"].as_i64().unwrap_or(0);
                let cached = u["cached_input_tokens"].as_i64().unwrap_or(0);
                out.push(Usage {
                    time: ts,
                    model: model.clone().unwrap_or_else(|| "unknown".into()),
                    input: (input - cached).max(0),
                    output: u["output_tokens"].as_i64().unwrap_or(0),
                    cache_read: cached,
                    cache_creation: u["cache_write_input_tokens"].as_i64().unwrap_or(0),
                });
            }
            _ => {}
        }
    }
}

fn scan_claude(path: &Path, out: &mut Vec<Usage>) {
    let Ok(f) = fs::File::open(path) else { return };
    for line in BufReader::new(f).lines().map_while(Result::ok) {
        if !line.contains("\"assistant\"") {
            continue;
        }
        let Ok(v) = serde_json::from_str::<serde_json::Value>(&line) else {
            continue;
        };
        if v["type"].as_str() != Some("assistant") {
            continue;
        }
        let Some(ts) = v["timestamp"].as_str().and_then(parse_ts) else {
            continue;
        };
        let u = &v["message"]["usage"];
        if !u.is_object() {
            continue;
        }
        out.push(Usage {
            time: ts,
            model: v["message"]["model"]
                .as_str()
                .unwrap_or("unknown")
                .to_string(),
            input: u["input_tokens"].as_i64().unwrap_or(0),
            output: u["output_tokens"].as_i64().unwrap_or(0),
            cache_read: u["cache_read_input_tokens"].as_i64().unwrap_or(0),
            cache_creation: u["cache_creation_input_tokens"].as_i64().unwrap_or(0),
        });
    }
}

// ---------------------------------------------------------------- 汇总

#[derive(Default)]
struct HostReport {
    host: String,
    files: usize,
    today: Bucket,
    window: Bucket,
    by_model: BTreeMap<String, (Bucket, Bucket)>, // (today, window)
    by_day: BTreeMap<NaiveDate, Bucket>,
}

pub fn run(days: i64, only_host: Option<&str>, daily: bool, json: bool) -> Result<()> {
    let days = days.max(1);
    let now = Local::now();
    let today: NaiveDate = now.date_naive();
    let since = today - chrono::Duration::days(days - 1);

    let base = root();
    let mut hosts: Vec<String> = Vec::new();
    if let Ok(rd) = fs::read_dir(&base) {
        for e in rd.flatten() {
            let p = e.path();
            if p.is_dir() {
                if let Some(name) = p.file_name().and_then(|s| s.to_str()) {
                    hosts.push(name.to_string());
                }
            }
        }
    }
    hosts.sort();
    if let Some(h) = only_host {
        hosts.retain(|x| x == h);
        if hosts.is_empty() {
            anyhow::bail!("镜像里没有主机 `{h}`，先跑 `ccr sync {h}`");
        }
    }
    if hosts.is_empty() {
        println!("还没有镜像数据。先跑一次：ccr sync");
        return Ok(());
    }

    let pricing = load_pricing();
    let mut reports: Vec<HostReport> = Vec::new();

    for host in &hosts {
        let mut files = Vec::new();
        collect_jsonl(&base.join(host).join("codex").join("sessions"), &mut files);
        collect_jsonl(&base.join(host).join("claude").join("projects"), &mut files);

        let mut usages = Vec::new();
        for f in &files {
            if f.to_string_lossy().contains("/claude/") {
                scan_claude(f, &mut usages);
            } else {
                scan_codex(f, &mut usages);
            }
        }

        let mut rep = HostReport {
            host: host.clone(),
            files: files.len(),
            ..Default::default()
        };
        for u in &usages {
            let d = u.time.date_naive();
            if d < since {
                continue;
            }
            let price = price_for(&pricing, &u.model);
            rep.window.add(u, price);
            rep.by_model.entry(u.model.clone()).or_default().1.add(u, price);
            rep.by_day.entry(d).or_default().add(u, price);
            if d == today {
                rep.today.add(u, price);
                rep.by_model.entry(u.model.clone()).or_default().0.add(u, price);
            }
        }
        reports.push(rep);
    }

    if json {
        return print_json(&reports, days, today, daily);
    }

    let mut total_today = Bucket::default();
    for r in &reports {
        total_today.merge(&r.today);
        println!(
            "\n### {}  (镜像 {} 个日志)",
            r.host,
            r.files
        );
        if r.files == 0 {
            println!("  暂无镜像日志");
            continue;
        }
        if r.window.req == 0 {
            println!("  近 {days} 天无用量记录");
            continue;
        }
        let has_today = r.today.req > 0;
        if has_today {
            println!(
                "  今日 {:<6} 次  ${:<9}  ·  近{}天 {:<6} 次  ${}",
                r.today.req,
                r.today.cost_str(),
                days,
                r.window.req,
                r.window.cost_str()
            );
            println!(
                "  {:<26}{:>8}{:>12}{:>12}{:>10}{:>10}{:>12}",
                "模型", "今日次数", "今日输入", "今日输出", "今日$", "窗口次数", "窗口$"
            );
            for (model, (t, w)) in &r.by_model {
                println!(
                    "  {:<26}{:>8}{:>12}{:>12}{:>10}{:>10}{:>12}",
                    model,
                    t.req,
                    thousands(t.input),
                    thousands(t.output),
                    t.cost_str(),
                    w.req,
                    w.cost_str()
                );
            }
            println!(
                "  {:<26}{:>8}{:>12}{:>12}{:>10}{:>10}{:>12}",
                "合计",
                r.today.req,
                thousands(r.today.input),
                thousands(r.today.output),
                r.today.cost_str(),
                r.window.req,
                r.window.cost_str()
            );
        } else {
            println!(
                "  今日 无记录  ·  近{}天 {} 次  ${}",
                days,
                r.window.req,
                r.window.cost_str()
            );
            println!(
                "  {:<26}{:>8}{:>12}{:>12}{:>10}",
                "模型", "次数", "输入", "输出", "花费$"
            );
            for (model, (_, w)) in &r.by_model {
                println!(
                    "  {:<26}{:>8}{:>12}{:>12}{:>10}",
                    model,
                    w.req,
                    thousands(w.input),
                    thousands(w.output),
                    w.cost_str()
                );
            }
            println!(
                "  {:<26}{:>8}{:>12}{:>12}{:>10}",
                "合计",
                r.window.req,
                thousands(r.window.input),
                thousands(r.window.output),
                r.window.cost_str()
            );
        }

        if daily {
            println!("\n  逐日：");
            println!("  {:<14}{:>8}{:>14}{:>12}{:>12}", "日期", "次数", "输入", "输出", "花费$");
            for (d, b) in &r.by_day {
                println!(
                    "  {:<14}{:>8}{:>14}{:>12}{:>12}",
                    d.format("%Y-%m-%d").to_string(),
                    b.req,
                    thousands(b.input),
                    thousands(b.output),
                    b.cost_str()
                );
            }
        }
    }

    println!("\n=== 全部服务器 · 今日合计 ===");
    println!(
        "  请求 {}  输入 {}  输出 {}  缓存读 {}  花费 ${}",
        total_today.req,
        thousands(total_today.input),
        thousands(total_today.output),
        thousands(total_today.cache_read),
        total_today.cost_str()
    );
    println!("\n注意：花费按 CC Switch 的 model_pricing 折算，不等同于中转站账单。");
    Ok(())
}

fn print_json(reports: &[HostReport], days: i64, today: NaiveDate, daily: bool) -> Result<()> {
    let mut total = Bucket::default();
    let hosts: Vec<serde_json::Value> = reports
        .iter()
        .map(|r| {
            total.merge(&r.today);
            let models: Vec<serde_json::Value> = r
                .by_model
                .iter()
                .map(|(m, (t, w))| {
                    serde_json::json!({
                        "model": m,
                        "today": {"requests": t.req, "input_tokens": t.input, "output_tokens": t.output,
                                  "cache_read_tokens": t.cache_read, "cost_usd": round4(t.cost)},
                        "window": {"requests": w.req, "input_tokens": w.input, "output_tokens": w.output,
                                   "cache_read_tokens": w.cache_read, "cost_usd": round4(w.cost)},
                    })
                })
                .collect();
            let mut obj = serde_json::json!({
                "host": r.host,
                "log_files": r.files,
                "today": {"requests": r.today.req, "input_tokens": r.today.input,
                          "output_tokens": r.today.output, "cache_read_tokens": r.today.cache_read,
                          "cost_usd": round4(r.today.cost)},
                "window": {"days": days, "requests": r.window.req, "input_tokens": r.window.input,
                           "output_tokens": r.window.output, "cache_read_tokens": r.window.cache_read,
                           "cost_usd": round4(r.window.cost)},
                "models": models,
            });
            if daily {
                obj["daily"] = serde_json::json!(r
                    .by_day
                    .iter()
                    .map(|(d, b)| serde_json::json!({
                        "date": d.format("%Y-%m-%d").to_string(),
                        "requests": b.req, "input_tokens": b.input, "output_tokens": b.output,
                        "cache_read_tokens": b.cache_read, "cost_usd": round4(b.cost)
                    }))
                    .collect::<Vec<_>>());
            }
            obj
        })
        .collect();

    let out = serde_json::json!({
        "generated_at": Local::now().to_rfc3339(),
        "today": today.format("%Y-%m-%d").to_string(),
        "days": days,
        "hosts": hosts,
        "today_total": {"requests": total.req, "input_tokens": total.input,
                        "output_tokens": total.output, "cache_read_tokens": total.cache_read,
                        "cost_usd": round4(total.cost)},
    });
    println!("{}", serde_json::to_string_pretty(&out)?);
    Ok(())
}

fn round4(v: f64) -> f64 {
    (v * 10000.0).round() / 10000.0
}

// ---------------------------------------------------------------- 工具

pub fn dir_size(dir: &Path) -> u64 {
    let Ok(rd) = fs::read_dir(dir) else { return 0 };
    let mut total = 0u64;
    for e in rd.flatten() {
        let p = e.path();
        if p.is_dir() {
            total += dir_size(&p);
        } else if let Ok(m) = e.metadata() {
            total += m.len();
        }
    }
    total
}

pub fn human_size(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "K", "M", "G", "T"];
    let mut v = bytes as f64;
    let mut i = 0;
    while v >= 1024.0 && i < UNITS.len() - 1 {
        v /= 1024.0;
        i += 1;
    }
    if i == 0 {
        format!("{}{}", v as u64, UNITS[i])
    } else {
        format!("{:.1}{}", v, UNITS[i])
    }
}

fn thousands(n: i64) -> String {
    let s = n.abs().to_string();
    let mut out = String::new();
    for (i, c) in s.chars().enumerate() {
        if i > 0 && (s.len() - i) % 3 == 0 {
            out.push(',');
        }
        out.push(c);
    }
    if n < 0 {
        format!("-{out}")
    } else {
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn thousands_separates() {
        assert_eq!(thousands(0), "0");
        assert_eq!(thousands(999), "999");
        assert_eq!(thousands(1000), "1,000");
        assert_eq!(thousands(1234567), "1,234,567");
        assert_eq!(thousands(-1234), "-1,234");
    }

    #[test]
    fn human_size_units() {
        assert_eq!(human_size(0), "0B");
        assert_eq!(human_size(1024), "1.0K");
        assert_eq!(human_size(1536), "1.5K");
        assert_eq!(human_size(1024 * 1024), "1.0M");
    }

    #[test]
    fn pricing_exact_and_prefix() {
        let mut p = HashMap::new();
        p.insert("gpt-6.1-sol".to_string(), (2.0, 10.0, 0.1, 0.0));
        p.insert("gpt-6".to_string(), (1.0, 1.0, 0.0, 0.0));
        assert_eq!(price_for(&p, "GPT-6.1-SOL"), Some((2.0, 10.0, 0.1, 0.0)));
        // 最长前缀优先：gpt-6.1-sol 比 gpt-6 更具体
        assert_eq!(
            price_for(&p, "gpt-6.1-sol-2026-01-01"),
            Some((2.0, 10.0, 0.1, 0.0))
        );
        assert_eq!(price_for(&p, "gpt-6-mini"), Some((1.0, 1.0, 0.0, 0.0)));
        assert_eq!(price_for(&p, "totally-unknown"), None);
        assert_eq!(price_for(&p, ""), None);
    }

    #[test]
    fn bucket_cost_math() {
        let mut b = Bucket::default();
        let u = Usage {
            time: Local::now(),
            model: "m".into(),
            input: 1_000_000,
            output: 1_000_000,
            cache_read: 0,
            cache_creation: 0,
        };
        b.add(&u, Some((2.0, 10.0, 0.0, 0.0)));
        assert!((b.cost - 12.0).abs() < 1e-9);
        assert!(!b.unpriced);
        assert_eq!(b.req, 1);
    }

    #[test]
    fn bucket_marks_unpriced() {
        let mut b = Bucket::default();
        let u = Usage {
            time: Local::now(),
            model: "nope".into(),
            input: 10,
            output: 10,
            cache_read: 0,
            cache_creation: 0,
        };
        b.add(&u, None);
        assert!(b.unpriced);
        assert_eq!(b.cost_str(), "n/a");
    }
}
