# CC Remote Usage - Agent Knowledge Base

**Generated:** 2026-10-21
**Type:** Rust CLI (Cargo, edition 2021)
**Size:** 9 source files (~3300 LOC including tests)

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
│   ├── main.rs             # CLI 定义与 hosts/sync/status/config 命令
│   ├── select.rs           # 本轮同步哪些主机：点名 / 定时任务列表 / 配置 / 探测 + 冷却 + 去重
│   ├── state.rs            # config.json / state.json 读写（原子写）、退避、运行锁、cc-switch 路径
│   ├── hosts.rs            # 主机发现（ssh config + cc-fleet + 配置）、并行探测、别名指纹去重
│   ├── ssh.rs              # 统一 ssh 参数（-T / RemoteCommand=none / BatchMode）与 stderr 摘要
│   ├── sync.rs             # tar-over-SSH 增量拉取 + 按 app 调 cc-switch 导入
│   ├── mirror.rs           # 镜像保留期清理
│   ├── report.rs           # 会话日志解析与去重、model-pricing 计价、表格/JSON 输出
│   └── daemon.rs           # launchd 定时任务安装/卸载/状态
├── docs/开发记录.md         # 调研结论、方案演进、踩坑、验证方法
└── README.md               # 用户向用法
```

## Build & Run Commands

```bash
cargo build --release --offline   # 本机 crates.io 403，必须离线
cargo test --offline              # 51 个单测 + 7 个端到端（tests/cli.rs）：
                                  # 单测里用本机 sh 直接跑远端脚本（macOS 上即 BSD 工具）；
                                  # 端到端用假 ssh / cc-switch 跑真二进制，覆盖 sync 全流程
```

安装（**本机现在是 npm 管理的**，见下方「安装方式」一节）：

```bash
npm i -g cc-remote-usage          # 推荐：自动匹配平台二进制，postinstall 从 Release 下载
```

⚠️ **不要**再跑 `cp target/release/ccr /opt/homebrew/bin/ccr`：该路径现在是 npm 的符号链接
（→ `../lib/node_modules/cc-remote-usage/bin/ccr.js`），`cp` 会穿透符号链接把 npm 的 shim
覆盖成 Rust 二进制，破坏 npm 包。本地开发直接用 `./target/release/ccr` 跑；
若要覆盖 PATH 里的二进制，先 `rm` 掉符号链接再 `cp`（注意备份）。

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
7. 远端脚本只用 POSIX 写法（`-newermt @epoch` 探测失败 → `-mmin` 回退；数字 / 体积要去掉
   BSD `wc -l`、`du -sh` 的前导空格）。改脚本后跑 `cargo test`，测试会在本机用 sh 真跑一遍。
8. 空文件列表、目录不存在都必须产出合法空 tar 归档，不要用 `|| exit 1` 破坏管道；tar 退出码 1 只有
   GNU tar 能当成功（"file changed as we read it"），bsdtar 的 1 表示有文件被跳过，必须算失败。
9. **导入按 app 逐个跑**（`--app codex` / `--app claude`），不要用 `--all`：它会扫本机
   Gemini / Pi 等不受环境变量控制的目录，把本机用量记到服务器头上。
10. 所有 ssh 调用都走 `ssh::command`（`-T -o BatchMode=yes -o RemoteCommand=none`）；远端脚本用
    `ssh::spawn_script` 经 `sh -s` 从 stdin 执行，不要再把脚本当命令行参数（fish / csh 登录 shell 会挂）。
11. `mirror::prune` 必须在导入之后跑；只动 `<host>/codex/sessions`、`<host>/claude/projects`，不跟随符号链接；
    保留期内 Codex 子代理会话的父会话链（`parent_thread_id`）不删；`config.json` 解析失败时整轮不清理。
12. `REMOTE_USAGE_HOSTS`（定时任务的主机列表）**不算点名**，照样受退避约束。
13. `ccr sync` 持 `sync.lock` 运行（dry-run 除外，交互多选期间临时释放）；`hosts --probe` 不占锁，
    落盘时才拿锁并基于最新状态合并；state.json 原子写、每台同步完立即落盘；探测失败要在任何提前
    return 之前落盘；本轮探测全部连不上时以非零退出码报错（定时任务日志要看得到）。
14. `report` 去重键：Claude `message.id`（CC Switch 记作 `session:<id>`）、Codex `response_id`，跨主机共享。
15. 主机名同时是 ssh 参数和镜像目录名，入口处用 `hosts::valid_host_id` 校验。

## 踩坑速查

| 现象 | 原因 | 处理 |
|---|---|---|
| `rsync: command not found` | 服务器没装 rsync | 用 tar 流式传输 |
| 某天没新日志却报错 | 空 tar / find 无输出 | 远端恒产出空归档 |
| 主机列表混进 git 远端 | cc-fleet 复用 Codex App SSH 列表 | 解析 ssh config 反查过滤 |
| 花费全是 n/a | `model-pricing.json` 缺该模型 | 正常，看板同样记 0 |
| 今日数字比预期少 | 会话时间戳 UTC，跨零点归昨日 | 预期行为 |
| `--force` 特别慢 | 它同时忽略断点全量重拉 | 只想解除冷却用 `--retry-now` |
| `Cannot execute command-line and remote command.` | ssh config 给该主机配了 `RemoteCommand` | 已用 `-o RemoteCommand=none` 覆盖 |
| macOS 远端探测永远 0 个文件 | BSD `wc -l` 输出带前导空格，`codex=  2` 被切开 | 脚本里 `$((c))` |
| `filesScanned` 远大于镜像文件数 | CC Switch 5.x `--all` 扫了本机其他 app | 按 `--app` 逐个导入 |
| report 的 Claude Token 是看板的 2~3 倍 | 一次响应按内容块拆成多行、usage 相同 | 按 `message.id` 去重 |
| 只看到 `ssh 退出码 255` | 旧版丢了 stderr | 现在附 stderr 最后一行（常见为 Mac 睡眠打断传输） |
| `ssh 退出码 1`，活跃主机偶发 | GNU tar 读文件时 agent 正在追加（"file changed as we read it"） | 远端脚本对 GNU tar 的 1 当成功（bsdtar 的 1 仍算失败） |
| 远端登录 shell 是 fish 时脚本语法错误 | 旧版把脚本当命令行交给登录 shell | 现在经 `sh -s` 从 stdin 执行 |
| 并行跑测试时锁偶尔"释放了还拿不到" | 别的测试线程 fork 子进程，exec 前短暂持有 fd 副本 | 测试里重试，生产无影响 |
| 同一台 Mac 被同步好几遍 | 多个别名指向同一台机器 | 探测指纹去重，`hosts --probe` 标为「重复」 |
| 本机 `find` 测试结论与远端不符 | Claude Code 的 shell 把 `find` 包成了 bfs | 验证 BSD 行为用 `/usr/bin/find` 或 `sh -c` |

## 安装方式

本机用 **npm 全局安装**（npm 前缀 = `/opt/homebrew`，与二进制路径一致）：

| 项 | 值 |
|---|---|
| npm 包 | `cc-remote-usage`（注意包名不是 `ccr`） |
| PATH 入口 | `/opt/homebrew/bin/ccr` → `../lib/node_modules/cc-remote-usage/bin/ccr.js`（node shim） |
| 平台二进制 | `<npm root -g>/cc-remote-usage/vendor/<platform>-<arch>/ccr`（postinstall 从 Release 下载） |
| 定时任务 | launchd 直接调 `/opt/homebrew/bin/ccr`，走 shim 也正常（node 在 `/opt/homebrew/bin`） |

升级：`npm i -g cc-remote-usage`。postinstall 按 `package.json` 的 version 拼下载地址，
所以 **必须先有对应 tag 的 GitHub Release**，再发 npm。npm 11 会对 install script 打印
`allow-scripts` 警告，但默认仍会执行；装完用 `ccr --version` 确认。

## 数据位置

| 路径 | 内容 |
|---|---|
| `~/.cc-switch/remote-usage/<host>/` | 会话日志镜像（`retain_days` 默认 7 天未更新即清理） |
| `~/.cc-switch/remote-usage/state.json` | 增量断点 + 失败次数 + 下次尝试 |
| `~/.cc-switch/remote-usage/config.json` | hosts / days / backoff / retain_days / cc-switch 路径 |
| `~/.cc-switch/remote-usage/sync.lock` | 运行锁 |
| `~/Library/LaunchAgents/com.fwmbam4.ccr-sync.plist` | 定时任务 |

## 与上游的约定

- 依赖 CC Switch 支持 `sessions sync-usage --app <codex|claude> --json`（实测 5.11.2；输出
  `imported` / `filesScanned` / `errors` 等）。
- `cc-switch` 路径：配置 > PATH > `/opt/homebrew/bin/cc-switch`。
- 若上游改 JSON 字段名，`sync.rs::parse_import` 缺 `imported` 时明确报错（不会悄悄记 0）；
  `errors` 数组作为警告显示，同步本身仍算成功。

## 姊妹项目

- [dsh-quota-bridge](../dsh-quota-bridge)：**额度**通路（DSH 账号余额 → CC Switch 卡片额度栏），
  走 `provider.meta.usage_script`；本项目做的是**用量统计**通路（会话日志 → 看板）。
  两者数据来源完全不同，改动时不要互相牵连。

## 发布流程

1. 版本对齐：`Cargo.toml` 与 `npm/package.json` 的 `version` 同步（CI 会用 tag 名覆盖 npm 版本）。
2. 打 tag 推送：
   ```bash
   git tag -a v0.1.1 -m "v0.1.1" && git push origin v0.1.1
   ```
3. `release.yml` 会自动：
   - 交叉编译 4 个目标（`x86_64/aarch64-unknown-linux-musl`、`x86_64/aarch64-apple-darwin`）
   - 把裸二进制 + `.tar.gz` 传到 GitHub Release（npm 的 postinstall 依赖这些文件名：
     `ccr-<target>`）
   - 若仓库配置了 `NPM_TOKEN` secret，顺带 `npm publish`；没配置则显式跳过（不报红）
4. 手动发布 npm（未配置 secret 时）：
   ```bash
   cd npm && npm login && npm publish --access public
   ```

⚠️ npm 包的 postinstall 按 `package.json` 的版本拼下载地址，所以**必须先有对应 tag 的 Release**，
再发 npm。
