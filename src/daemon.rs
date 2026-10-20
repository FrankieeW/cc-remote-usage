//! launchd 定时任务管理（macOS）。

use crate::root;
use anyhow::{bail, Context, Result};
use std::fs;
use std::path::PathBuf;
use std::process::Command;

const LABEL: &str = "com.fwmbam4.ccr-sync";
const LEGACY_LABEL: &str = "com.fwmbam4.ccswitch-remote-usage";

fn plist_path() -> PathBuf {
    crate::home()
        .join("Library")
        .join("LaunchAgents")
        .join(format!("{LABEL}.plist"))
}

fn target() -> String {
    format!("gui/{}", uid())
}

fn uid() -> String {
    Command::new("id")
        .arg("-u")
        .output()
        .ok()
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|s| s.trim().to_string())
        .unwrap_or_else(|| "501".to_string())
}

fn exe_path() -> Result<PathBuf> {
    std::env::current_exe().context("无法确定 ccr 自身路径")
}

pub fn install(interval: u64, hosts: &[String]) -> Result<()> {
    let exe = exe_path()?;
    let dir = root();
    fs::create_dir_all(&dir)?;
    fs::create_dir_all(crate::home().join("Library").join("LaunchAgents"))?;

    let hosts_env = if hosts.is_empty() {
        String::new()
    } else {
        format!(
            "        <key>REMOTE_USAGE_HOSTS</key>\n        <string>{}</string>\n",
            hosts.join(" ")
        )
    };

    let plist = format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>Label</key>
    <string>{label}</string>

    <key>ProgramArguments</key>
    <array>
        <string>{exe}</string>
        <string>sync</string>
        <string>--all</string>
        <string>--quiet</string>
    </array>

    <key>EnvironmentVariables</key>
    <dict>
        <key>PATH</key>
        <string>/opt/homebrew/bin:/usr/local/bin:/usr/bin:/bin:/usr/sbin:/sbin</string>
{hosts}</dict>

    <key>StartInterval</key>
    <integer>{interval}</integer>

    <key>RunAtLoad</key>
    <true/>

    <key>StandardOutPath</key>
    <string>{out}</string>
    <key>StandardErrorPath</key>
    <string>{err}</string>
</dict>
</plist>
"#,
        label = LABEL,
        exe = exe.display(),
        hosts = hosts_env,
        interval = interval.max(60),
        out = dir.join("launchd.out.log").display(),
        err = dir.join("launchd.err.log").display(),
    );

    // 清掉旧的任务（包括之前 bash 版的）
    for label in [LABEL, LEGACY_LABEL] {
        let _ = Command::new("launchctl")
            .args(["bootout", &format!("{}/{}", target(), label)])
            .output();
    }
    let legacy = crate::home()
        .join("Library")
        .join("LaunchAgents")
        .join(format!("{LEGACY_LABEL}.plist"));
    if legacy.exists() {
        let _ = fs::remove_file(&legacy);
    }

    let path = plist_path();
    fs::write(&path, plist)?;

    let out = Command::new("launchctl")
        .args(["bootstrap", &target(), path.to_str().unwrap_or_default()])
        .output()
        .context("launchctl bootstrap 失败")?;
    if !out.status.success() {
        bail!(
            "launchctl bootstrap 失败: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }

    println!("✓ 已安装定时任务 {LABEL}");
    println!("  每 {} 分钟同步一次：{}", interval.max(60) / 60, exe.display());
    println!("  主机：{}", if hosts.is_empty() { "自动发现（可达且有日志）".into() } else { hosts.join(", ") });
    println!("  日志：{}", dir.join("launchd.err.log").display());
    println!("  卸载：ccr daemon uninstall");
    Ok(())
}

pub fn uninstall() -> Result<()> {
    let _ = Command::new("launchctl")
        .args(["bootout", &format!("{}/{}", target(), LABEL)])
        .output();
    let path = plist_path();
    if path.exists() {
        fs::remove_file(&path)?;
    }
    println!("✓ 已卸载定时任务 {LABEL}");
    Ok(())
}

pub fn status() -> Result<()> {
    let path = plist_path();
    if !path.exists() {
        println!("定时任务未安装。安装：ccr daemon install");
        return Ok(());
    }
    let out = Command::new("launchctl").arg("list").output()?;
    let text = String::from_utf8_lossy(&out.stdout);
    println!("plist：{}", path.display());
    match text.lines().find(|l| l.contains(LABEL)) {
        Some(line) => {
            let mut it = line.split_whitespace();
            let pid = it.next().unwrap_or("-");
            let code = it.next().unwrap_or("-");
            println!("状态：已加载（PID {pid}，上次退出码 {code}）");
        }
        None => println!("状态：plist 存在但未加载（跑一次 ccr daemon install）"),
    }
    let err = root().join("launchd.err.log");
    if let Ok(t) = fs::read_to_string(&err) {
        if !t.trim().is_empty() {
            println!("\n最近错误日志（{}）：", err.display());
            for l in t.lines().rev().take(5).collect::<Vec<_>>().into_iter().rev() {
                println!("  {l}");
            }
        }
    }
    Ok(())
}
