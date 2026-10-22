use crate::{ssh, state};
use anyhow::{bail, Context, Result};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;

pub struct Importer {
    pub bin: PathBuf,
    pub version: String,
}

impl Importer {
    pub fn detect(cfg: &state::Config) -> Result<Self> {
        let selected = state::cc_switch_bin_for(cfg);
        let resolved = if Path::new(&selected).components().count() == 1 {
            state::which(&selected).with_context(|| format!("PATH 中找不到 {selected}"))?
        } else {
            selected
        };
        let bin = std::fs::canonicalize(&resolved)
            .with_context(|| format!("找不到 cc-switch：{resolved}"))?;
        let child = Command::new(&bin)
            .arg("--version")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .with_context(|| format!("无法运行 {} --version", bin.display()))?;
        let watchdog = ssh::Watchdog::arm(child.id(), Duration::from_secs(15));
        let result = child.wait_with_output();
        let timed_out = watchdog.disarm();
        let output = result.context("读取 cc-switch 版本失败")?;
        if timed_out && output.status.code().is_none() {
            bail!("cc-switch --version 超过 15 秒没有返回");
        }
        if !output.status.success() {
            bail!(
                "cc-switch --version 失败：{}",
                ssh::last_line(&String::from_utf8_lossy(&output.stderr)).unwrap_or_default()
            );
        }
        let text = String::from_utf8_lossy(&output.stdout);
        let words: Vec<_> = text.split_whitespace().collect();
        let ["cc-switch", version] = words.as_slice() else {
            bail!("无法识别 cc-switch --version 的输出");
        };
        Ok(Self {
            bin,
            version: (*version).to_owned(),
        })
    }

    pub fn check(cfg: &state::Config) -> Result<Self> {
        let importer = Self::detect(cfg)?;
        if let Some(expected) = &cfg.cc_switch_version {
            if expected != &importer.version {
                bail!(
                    "cc-switch 版本不匹配：已锁定 {expected}，当前为 {}。\
                     请先确认与桌面版兼容，再运行 ccr config --pin-cc-switch 重新锁定",
                    importer.version
                );
            }
        }
        Ok(importer)
    }
}
