# CC Remote Usage - Agent Knowledge Base

**Generated:** 2026-10-21
**Type:** Rust CLI (Cargo, edition 2021)
**Size:** 5 source files (~1700 LOC including tests)

## Overview

`ccr` 把服务器上 Codex / Claude Code agent 的会话日志增量拉回本机镜像目录，再用作用域化的
`CODEX_HOME` / `CLAUDE_CONFIG_DIR` 调 `cc-switch sessions sync-usage`，让服务器用量并入
CC Switch 的「用量」看板。

不写 CC Switch 数据库，不复制 OAuth 凭据，只走它自己的 CLI。

## Project Structure

```
cc-remote-usage/
├── Cargo.toml              # clap / serde_json / chrono / dialoguer / anyhow
├── src/
│   ├── main.rs             # CLI 定义、配置与状态、退避策略、sync/status/config 命令
│   ├── hosts.rs            # SSH 主机发现（ssh config + cc-fleet + 配置）、并行探测
│   ├── sync.rs             # tar-over-SSH 增量拉取 + 调 cc-switch 导入
│   ├── report.rs           # 会话日志解析、model-pricing 计价、表格/JSON 输出
│   └── daemon.rs           # launchd 定时任务安装/卸载/状态
├── docs/开发记录.md         # 调研结论、方案演进、踩坑、验证方法
└── README.md               # 用户向用法
```

## Build & Run Commands

```bash
cargo build --release --offline   # 本机 crates.io 403，必须离线
cargo test --offline              # 10 个单测：退避、冷却、计价、格式化
cp target/release/ccr /opt/homebrew/bin/ccr
```

```bash
ccr hosts --probe        # 发现 + 探测
ccr sync --all --quiet   # 定时任务用
ccr report --days 7
ccr status
ccr daemon install --interval 900
```

## 关键不变量（改动前务必确认）

1. **导入必须走 `cc-switch sessions sync-usage`**，不要直接 insert 进 `proxy_request_logs`。
2. **`last_epoch` 只在成功时推进**；失败推进会导致增量窗口漏文件。
3. **退避状态与同步状态同存 `state.json`**，`record_failure` 只累加不覆盖 `last_epoch`。
4. **命令行点名的主机必须无视冷却**（用户明确要求重试），`--force` / `--retry-now` 同理。
5. **探测阶段就要剔除冷却主机**，否则退避等于没做（连接超时发生在探测里）。
6. **镜像目录只能通过环境变量作用域**，不要改 CC Switch 的全局配置。
7. 远端脚本要兼容 BSD find（`-newermt @epoch` 探测失败 → `-mmin` 回退）。
8. 空文件列表必须产出合法空 tar 归档（GNU tar 行为），不要用 `|| exit 1` 破坏管道。

## 踩坑速查

| 现象 | 原因 | 处理 |
|---|---|---|
| `rsync: command not found` | 服务器没装 rsync | 用 tar 流式传输 |
| 某天没新日志却报错 | 空 tar / find 无输出 | 远端恒产出空归档 |
| 主机列表混进 git 远端 | cc-fleet 复用 Codex App SSH 列表 | 解析 ssh config 反查过滤 |
| 花费全是 n/a | `model-pricing.json` 缺该模型 | 正常，看板同样记 0 |
| 今日数字比预期少 | 会话时间戳 UTC，跨零点归昨日 | 预期行为 |
| `--force` 特别慢 | 它同时忽略断点全量重拉 | 只想解除冷却用 `--retry-now` |

## 数据位置

| 路径 | 内容 |
|---|---|
| `~/.cc-switch/remote-usage/<host>/` | 会话日志镜像（5 天自动清理） |
| `~/.cc-switch/remote-usage/state.json` | 增量断点 + 失败次数 + 下次尝试 |
| `~/.cc-switch/remote-usage/config.json` | hosts / days / backoff / cc-switch 路径 |
| `~/Library/LaunchAgents/com.fwmbam4.ccr-sync.plist` | 定时任务 |

## 与上游的约定

- 依赖 CC Switch ≥ 4.0（`sessions sync-usage --json` 输出 `imported` / `filesScanned`）。
- `cc-switch` 路径：配置 > PATH > `/opt/homebrew/bin/cc-switch`。
- 若上游改 JSON 字段名，`sync.rs::import` 会以 "输出不是预期 JSON" 明确报错。
