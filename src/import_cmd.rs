use crate::{importer::Importer, select, state, sync};
use anyhow::{bail, Result};

pub fn run(hosts: Vec<String>, quiet: bool) -> Result<()> {
    select::check_hosts(&hosts)?;
    let cfg = state::read_config()?;
    let Some(_lock) = state::try_lock_runs()? else {
        if !quiet {
            println!("另一个 ccr sync / import 正在运行，本次跳过。");
        }
        return Ok(());
    };
    let importer = Importer::check(&cfg)?;
    let mut state = state::load_state();
    let mut failures = 0;
    let mut imported = 0;
    for host in &hosts {
        let outcome = sync::import_host(host, &importer.bin);
        match &outcome.err {
            Some(error) => {
                failures += 1;
                state::record_failure(&cfg, &mut state, host, error);
            }
            None => {
                let checkpoint = state.hosts.get(host).map_or(0, |entry| entry.last_epoch);
                state::record_success(
                    &mut state,
                    host,
                    checkpoint,
                    outcome.imported,
                    outcome.scanned,
                );
                imported += outcome.imported;
            }
        }
        state::save_state(&state)?;
        if quiet {
            if let Some(error) = &outcome.err {
                eprintln!("{host}: {error}");
            }
            for warning in sync::warning_summary(&outcome.warnings) {
                eprintln!("{host}: cc-switch：{warning}");
            }
        } else {
            println!("{host}（本地镜像）");
            crate::print_outcome(&cfg, &state, &outcome);
        }
    }
    if !quiet {
        println!(
            "\n本地导入：{}/{} 台成功，共导入 {imported} 条；远端拉取断点保持不变。",
            hosts.len() - failures,
            hosts.len()
        );
    }
    if failures > 0 {
        bail!("{failures} 台本地镜像导入失败");
    }
    Ok(())
}
