//! 本机镜像目录维护：按保留期清理旧日志。

use std::collections::{HashMap, HashSet};
use std::fs;
use std::io::{BufRead, BufReader, Read};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

/// 镜像里由 ccr 管理的子目录：只在这里面删文件，不碰根目录下的 state / config / 日志。
const MANAGED: [[&str; 2]; 2] = [["codex", "sessions"], ["claude", "projects"]];

#[derive(Debug, Default, PartialEq)]
pub struct Pruned {
    pub files: usize,
    pub bytes: u64,
}

/// 删除 `<root>/<host>/{codex/sessions,claude/projects}` 下 mtime 早于保留期的 `.jsonl`，
/// 并清掉因此变空的子目录。
///
/// 会话日志只追加不改写，mtime 旧就说明整份文件都是旧记录，CC Switch 早已导入
/// （调用方保证先导入后清理）；它按文件路径记的增量断点也不受影响——远端文件以后
/// 再有追加，会被整份重新拉回、从原断点继续导入。
/// 例外：保留期内的 Codex 子代理会话（fork）的父会话链一并保留，缺了父会话
/// CC Switch 会暂缓导入子会话。
/// `retain_days == 0` 表示不清理；符号链接一律跳过、不跟随。
pub fn prune(root: &Path, retain_days: u64, now: SystemTime) -> Pruned {
    let mut acc = Pruned::default();
    if retain_days == 0 {
        return acc;
    }
    let Some(cutoff) = now.checked_sub(Duration::from_secs(retain_days.saturating_mul(86_400)))
    else {
        return acc;
    };
    let Ok(hosts) = fs::read_dir(root) else {
        return acc;
    };
    for host in hosts.flatten() {
        if !host.file_type().map(|t| t.is_dir()).unwrap_or(false) {
            continue;
        }
        for [top, sub] in MANAGED {
            let top_dir = host.path().join(top);
            let dir = top_dir.join(sub);
            if is_real_dir(&top_dir) && is_real_dir(&dir) {
                // 只有 Codex rollout 有父会话（parent_thread_id），Claude 目录不必逐个读首行
                prune_dir(&dir, cutoff, top == "codex", &mut acc);
            }
        }
    }
    acc
}

/// 是目录且本身不是符号链接
fn is_real_dir(p: &Path) -> bool {
    fs::symlink_metadata(p)
        .map(|m| m.file_type().is_dir())
        .unwrap_or(false)
}

struct Entry {
    path: PathBuf,
    old: bool,
    len: u64,
}

fn prune_dir(dir: &Path, cutoff: SystemTime, track_parents: bool, acc: &mut Pruned) {
    let mut entries = Vec::new();
    collect(dir, cutoff, &mut entries);
    if !entries.iter().any(|e| e.old) {
        return;
    }
    let keep = if track_parents {
        ancestors_of_retained(&entries)
    } else {
        HashSet::new()
    };
    for e in entries.iter().filter(|e| e.old && !keep.contains(&e.path)) {
        if fs::remove_file(&e.path).is_ok() {
            acc.files += 1;
            acc.bytes += e.len;
        }
    }
    remove_empty_subdirs(dir);
}

/// 递归收集 .jsonl（只认普通文件和真目录，符号链接一律跳过）
fn collect(dir: &Path, cutoff: SystemTime, out: &mut Vec<Entry>) {
    let Ok(rd) = fs::read_dir(dir) else { return };
    for e in rd.flatten() {
        let Ok(ft) = e.file_type() else { continue };
        let path = e.path();
        if ft.is_dir() {
            collect(&path, cutoff, out);
        } else if ft.is_file() && path.extension().is_some_and(|x| x == "jsonl") {
            let Ok(meta) = e.metadata() else { continue };
            let old = meta.modified().map(|m| m < cutoff).unwrap_or(false);
            out.push(Entry {
                path,
                old,
                len: meta.len(),
            });
        }
    }
}

/// 保留期内文件的父会话链上的旧文件（沿 `parent_thread_id` 一路往上）。
/// 会话 id 就是 rollout 文件名末尾的 UUID。
fn ancestors_of_retained(entries: &[Entry]) -> HashSet<PathBuf> {
    let by_id: HashMap<&str, &Entry> = entries
        .iter()
        .filter_map(|e| Some((session_id(&e.path)?, e)))
        .collect();
    let mut keep = HashSet::new();
    let mut visited = HashSet::new();
    let mut frontier: Vec<String> = entries
        .iter()
        .filter(|e| !e.old)
        .filter_map(|e| parent_id(&e.path))
        .collect();
    while let Some(id) = frontier.pop() {
        if !visited.insert(id.clone()) {
            continue;
        }
        let Some(parent) = by_id.get(id.as_str()) else {
            continue;
        };
        if parent.old {
            keep.insert(parent.path.clone());
        }
        frontier.extend(parent_id(&parent.path));
    }
    keep
}

fn session_id(p: &Path) -> Option<&str> {
    let stem = p.file_stem()?.to_str()?;
    stem.get(stem.len().checked_sub(36)?..)
}

/// rollout 首行 session_meta 里的 `parent_thread_id`（实测首行 20~50KB，读 1MB 封顶）
fn parent_id(p: &Path) -> Option<String> {
    let f = fs::File::open(p).ok()?;
    let mut line = String::new();
    BufReader::new(f.take(1 << 20)).read_line(&mut line).ok()?;
    let v: serde_json::Value = serde_json::from_str(&line).ok()?;
    if v["type"].as_str() != Some("session_meta") {
        return None;
    }
    v["payload"]["parent_thread_id"]
        .as_str()
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

/// 自底向上删空目录（`remove_dir` 遇到非空目录会失败，正好只删空的），托管目录本身保留
fn remove_empty_subdirs(dir: &Path) {
    let Ok(rd) = fs::read_dir(dir) else { return };
    for e in rd.flatten() {
        if e.file_type().map(|t| t.is_dir()).unwrap_or(false) {
            remove_empty_subdirs(&e.path());
            let _ = fs::remove_dir(e.path());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::tests::{scratch, touch};
    use std::path::PathBuf;

    const DAY: Duration = Duration::from_secs(86_400);

    #[test]
    fn prunes_only_old_jsonl_inside_managed_dirs() {
        let root = scratch("prune");
        let host = root.join("netcup");
        let old_codex = host.join("codex/sessions/2026/09/01/rollout-old.jsonl");
        let new_codex = host.join("codex/sessions/2026/10/20/rollout-new.jsonl");
        let old_claude = host.join("claude/projects/proj/a.jsonl");
        let old_note = host.join("codex/sessions/2026/09/02/notes.txt");
        let old_state = root.join("state.json");
        let old_unmanaged = host.join("elsewhere/x.jsonl");
        for p in [
            &old_codex,
            &old_claude,
            &old_note,
            &old_state,
            &old_unmanaged,
        ] {
            touch(p, DAY * 30);
        }
        touch(&new_codex, DAY);

        let pruned = prune(&root, 7, SystemTime::now());

        assert_eq!(pruned.files, 2);
        assert_eq!(pruned.bytes, 6);
        assert!(!old_codex.exists() && !old_claude.exists());
        assert!(new_codex.exists(), "保留期内的文件不动");
        assert!(old_note.exists(), "只删 .jsonl");
        assert!(
            old_state.exists() && old_unmanaged.exists(),
            "托管目录以外不动"
        );
        assert!(
            !host.join("codex/sessions/2026/09/01").exists(),
            "清空的子目录一并删除"
        );
        assert!(host.join("claude/projects").exists(), "托管目录本身保留");
    }

    /// 写一个只有 session_meta 首行的 Codex rollout（文件名末尾 36 位即会话 id）
    fn rollout(dir: &Path, id: &str, parent: Option<&str>, age: Duration) -> PathBuf {
        let p = dir.join(format!("rollout-2026-10-01T00-00-00-{id}.jsonl"));
        let parent = parent.map_or("null".to_string(), |p| format!("\"{p}\""));
        fs::create_dir_all(dir).unwrap();
        fs::write(
            &p,
            format!(
                "{{\"type\":\"session_meta\",\"payload\":{{\"id\":\"{id}\",\"parent_thread_id\":{parent}}}}}\n"
            ),
        )
        .unwrap();
        let f = fs::File::options().write(true).open(&p).unwrap();
        f.set_modified(SystemTime::now() - age).unwrap();
        p
    }

    #[test]
    fn keeps_the_parent_chain_of_retained_subagent_rollouts() {
        // Codex 子代理会话（fork）要靠父会话才能导入（缺了 CC Switch 会暂缓导入）：
        // 父会话很久没动、子会话还在跑时，父会话链不能删
        let root = scratch("prune-parents");
        let dir = root.join("h/codex/sessions/2026/10");
        let grandparent = "01a0aaaa-0000-7000-8000-000000000001";
        let parent = "01a0bbbb-0000-7000-8000-000000000002";
        let recent_mid = "01a0cccc-0000-7000-8000-000000000003";
        let gp_of_mid = "01a0dddd-0000-7000-8000-000000000004";
        let gp = rollout(&dir, grandparent, None, DAY * 40);
        let p = rollout(&dir, parent, Some(grandparent), DAY * 30);
        let child = rollout(
            &dir,
            "01a15a00-0000-7000-8000-000000000005",
            Some(parent),
            DAY,
        );
        // 链条中间是新文件、更上面是旧文件，也要沿着链往上保留
        let old_top = rollout(&dir, gp_of_mid, None, DAY * 30);
        let mid = rollout(&dir, recent_mid, Some(gp_of_mid), DAY);
        let unrelated = rollout(&dir, "01a0eeee-0000-7000-8000-000000000006", None, DAY * 30);

        let pruned = prune(&root, 7, SystemTime::now());

        for kept in [&gp, &p, &child, &old_top, &mid] {
            assert!(kept.exists(), "{} 应保留", kept.display());
        }
        assert!(!unrelated.exists(), "无关的旧会话照常清理");
        assert_eq!(pruned.files, 1);
    }

    #[test]
    fn zero_retention_keeps_everything() {
        let root = scratch("prune-zero");
        let f = root.join("h/codex/sessions/old.jsonl");
        touch(&f, DAY * 365);
        assert_eq!(prune(&root, 0, SystemTime::now()), Pruned::default());
        assert!(f.exists());
    }

    #[test]
    fn does_not_follow_symlinks_out_of_the_mirror() {
        use std::os::unix::fs::symlink;
        let root = scratch("prune-symlink");
        let outside = scratch("prune-symlink-outside");
        let victim = outside.join("codex/sessions/keep.jsonl");
        touch(&victim, DAY * 30);
        fs::create_dir_all(root.join("h1/codex/sessions")).unwrap();
        fs::create_dir_all(root.join("h3")).unwrap();
        // 链接分别出现在：托管目录内部、主机目录本身、中间层目录
        symlink(
            outside.join("codex/sessions"),
            root.join("h1/codex/sessions/link"),
        )
        .unwrap();
        symlink(&outside, root.join("h2")).unwrap();
        symlink(outside.join("codex"), root.join("h3/codex")).unwrap();

        prune(&root, 7, SystemTime::now());

        assert!(victim.exists(), "镜像里的符号链接不能把清理带到外面去");
    }
}
