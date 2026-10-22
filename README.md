# ccr — 服务器 agent 用量 → CC Switch 统一看板

[![release](https://github.com/FrankieeW/cc-remote-usage/actions/workflows/release.yml/badge.svg)](https://github.com/FrankieeW/cc-remote-usage/actions/workflows/release.yml)

CC Switch 的「用量」看板只扫**本机**的会话日志（`~/.codex/sessions`、`~/.claude/projects`，受
`CODEX_HOME` / `CLAUDE_CONFIG_DIR` 控制），所以跑在 VPS / 服务器上的 Codex、Claude Code
agent 消耗不会出现在里面。

`ccr` 把各服务器上的会话日志增量拉回本机镜像目录，再用作用域环境变量调用
`cc-switch sessions sync-usage`，让服务器用量并入同一份统计 —— 看板里就能看到
「今日用量」包含服务器上的 agent。

## 安装

**npm（推荐，自动匹配平台二进制）**

```bash
npm i -g cc-remote-usage
ccr --version
```

**直接下载二进制**：到 [Releases](https://github.com/FrankieeW/cc-remote-usage/releases) 取
`ccr-<target>`（macOS / Linux，arm64 / x64），`chmod +x` 后丢进 `PATH`。

**本地构建**

```bash
cargo build --release --offline   # 离线；去掉 --offline 也可
./target/release/ccr --help
```

npm 安装的 `/opt/homebrew/bin/ccr` 是 Node shim 的符号链接，不要用 `cp` 覆盖它。

## 用法

```bash
ccr hosts                 # 自动发现候选主机（~/.ssh/config + CC Switch Fleet + 配置）
ccr hosts --probe         # 并行探测连通性、近 3 天日志数量与体积（跳过冷却中的机器）
ccr hosts --probe --force # 无视冷却，强制探测全部候选
ccr sync                  # 交互多选要同步的服务器 → 增量同步 → 导入 CC Switch
ccr sync netcup           # 指定主机（点名的机器不受冷却限制）
ccr sync --all --quiet    # 全自动（定时任务用：只挑可达且有日志的主机）
ccr sync --retry-now      # 立刻重试冷却中的机器，仍走增量
ccr sync --all --force    # 无视冷却 + 忽略断点全量重拉
ccr import netcup         # 只重试已有镜像的导入，不连接远端、不推进拉取断点
ccr report --days 7       # 按服务器 / 模型汇总 Token 与折算花费
ccr report --days 7 --daily   # 加逐日明细
ccr report --json         # 机器可读
ccr status                # 上次同步、连续失败次数、下次尝试时间、镜像占用
ccr daemon install        # 装 launchd 定时任务（默认每 15 分钟）
ccr daemon status
ccr daemon uninstall
ccr config --set-hosts netcup,frankie-vps   # 固定默认主机（不再交互）
ccr config --set-days 3                     # 首次回溯天数
ccr config --set-backoff 15,360             # 退避：基础 15 分钟，上限 6 小时
ccr config --set-retain-days 7              # 镜像保留天数（0 = 不清理）
ccr config --pin-cc-switch                 # 锁定当前安装的 CLI 版本
ccr config --unpin-cc-switch               # 取消版本锁定
```

## 额外 Codex 日志目录（显式配置）

默认只扫描 SSH 登录用户的 `~/.codex/sessions` 和 `~/.claude/projects`。
Pharos 等编排器的 main、worker、verifier、subagent 可能分别使用自己的 `CODEX_HOME`，
需要用户指定这些会话所在的远端绝对路径；ccr 不会自动搜索项目或整台服务器。

```bash
ccr config --add-codex-root 'server=/srv/pharos/runtime/projects'
ccr sync server
ccr config --clear-codex-roots server
```

`--add-codex-root` 可重复指定。ccr 只拉取配置范围内 `sessions/` 和 `archived_sessions/`
下面的 `.jsonl`，不复制 `auth.json` 或 `config.toml`；跳过下级 `materials`、`.git`、
`.venv`、`node_modules`，避免把材料中的历史副本当作当前 actor 日志。需要同步这些目录时，
可以显式配置其中的会话目录本身。

新增或更改目录列表后，下一次同步会先完整回补这些目录，再恢复增量同步；导入失败不会
把回补标成完成。不同来源的同名文件分开保存，并整理到 CC Switch 能扫描到的目录深度。
有额外目录配置的主机也会参与 `sync --all`，默认目录没有近期日志时不会被跳过；
`hosts --probe` 的文件数仍只统计默认目录，额外目录会单独标注。

## 导入器版本与失败重试

先确认桌面版与 CLI 兼容，再执行 `ccr config --pin-cc-switch`。锁定记录在配置的
`cc_switch_version` 中；每轮同步在探测 SSH、拉取日志之前检查 CLI 版本，并固定本轮
执行的二进制路径。版本不一致会停止，确认新版本兼容后可重新锁定。版本锁定防止后续
CLI 意外升级，不会自动判断桌面版与数据库是否兼容。

日志已拉取、但导入失败时可运行 `ccr import <主机...>`，直接重试本地镜像。
它沿用同步锁、清除成功主机的失败状态，但保持 `last_epoch` 不变；下次正常同步仍会补齐
远端新增日志。普通同步及本地导入只要有一台失败，都返回非零退出码，`--quiet` 也一样。
配置文件解析失败时停止操作，防止绕过版本锁定。

首次同步默认只回溯两天，之后的增量不会补回更早且未修改的文件。
需要完整历史时显式运行 `ccr sync <主机> --force`；历史用量由 CC Switch 去重，
成功导入后仍会按镜像保留期清理本地旧文件。

## 失败退避（超时不反复赔）

连不上的机器不会每轮都重试，而是按连续失败次数指数退避：

| 连续失败 | 1 | 2 | 3 | 4 | 5 | 6+ |
|---|---|---|---|---|---|---|
| 下次尝试间隔 | 15 分钟 | 30 分钟 | 1 小时 | 2 小时 | 4 小时 | 6 小时（上限） |

- 一次成功同步会把计数和冷却清零。
- 本轮探测的主机**全部**连不上时，`ccr sync` 以非零退出码报错（多半是本机网络 / SSH agent
  的问题），定时任务日志和 `ccr daemon status` 里能直接看到原因。
- 冷却中的机器在 `ccr sync --all`、`ccr hosts --probe` 里直接跳过，不发起 SSH（每轮省掉 N 个超时等待）。
- 在命令行**点名**的主机（`ccr sync yeats-vps`）不受冷却限制；`ccr daemon install --hosts`
  指定的列表来自定时任务，仍受冷却约束。
- 想立刻重试全部：`ccr sync --all --retry-now`；连增量断点也一起丢掉用 `--force`。
- 间隔可调：`ccr config --set-backoff <基础分钟>,<上限分钟>`，写在 `config.json` 的
  `backoff_base_min` / `backoff_max_min`。
- `ccr status` 会列出每台的连续失败次数、下次尝试时间和最近一次错误原因。

## 工作原理

```
服务器                                本机
~/.codex/sessions/**.jsonl  ──ssh tar──▶  ~/.cc-switch/remote-usage/<host>/codex/sessions/
~/.claude/projects/**.jsonl ──ssh tar──▶  ~/.cc-switch/remote-usage/<host>/claude/projects/
                                              │
                                              ▼
                       CODEX_HOME=<host>/codex CLAUDE_CONFIG_DIR=<host>/claude \
                         cc-switch sessions sync-usage --app codex|claude --json
                                              │
                                              ▼
                       ~/.cc-switch/cc-switch.db: proxy_request_logs(data_source=codex_session/claude...)
                                              │
                                              ▼
                                   CC Switch「用量」看板 / ccr report
```

- **增量**：按文件 mtime 过滤（`find -newermt @<上次同步时间 - 5 分钟>`，多看 5 分钟抵消时钟偏差），
  状态存在 `~/.cc-switch/remote-usage/state.json`，每台同步完立即落盘；`--force` 可全量重拉。
- **不需要 rsync**：服务器上通常没装，改用 `tar` 流式传输（gzip 压缩后走 SSH）。
- **不碰 CC Switch 数据库**：只调用它自己的 CLI，等价于它内建的会话日志同步。
- **只导入 Codex / Claude**：按 `--app` 逐个导入。`--all` 会连带扫本机 Gemini / Pi 等
  不受环境变量控制的目录，把本机用量算进服务器的导入数。
- **去重**：CC Switch 自己按 `session_log_sync` 的 byte offset 去重，重复导入无副作用。
- **别名去重**：几个别名指向同一台机器（如 `macmini` / `macmini-lan` / `macmini-tailscale`）时，
  探测阶段按会话文件指纹识别，只同步一次；指向本机的别名直接跳过。
- **不会撞车**：`ccr sync` 与 `ccr import` 共用 `sync.lock`，定时任务碰上手动操作会自动跳过本轮；
  交互多选等待期间不占锁。卡死的远端不会一直占着锁：单台探测 2 分钟、单个目录传输 60 分钟、
  一次 cc-switch 导入 15 分钟，超时即中止（下轮重试）。

## 数据位置

| 路径 | 内容 |
|---|---|
| `~/.cc-switch/remote-usage/<host>/` | 会话日志镜像（自动清理 7 天未更新的文件，`retain_days` 可调） |
| `~/.cc-switch/remote-usage/state.json` | 每台主机的增量断点与上次结果 |
| `~/.cc-switch/remote-usage/config.json` | 默认主机、额外 Codex 目录、回溯天数、退避、保留天数、CLI 路径和版本锁定 |
| `~/.cc-switch/remote-usage/sync.lock` | 运行锁（防止两个同步同时跑） |
| `~/Library/LaunchAgents/com.fwmbam4.ccr-sync.plist` | 定时任务（`ccr daemon install` 生成） |
| `~/.cc-switch/remote-usage/launchd.err.log` | 定时任务日志 |

## 服务器要求

- Linux 或 macOS（GNU / BSD 的 `find`、`tar` 都行；不认 `-newermt @epoch` 的 find 会退回 `-mmin`）
- SSH 免密（公钥）可用，`BatchMode=yes` 下能跑
- ssh config 里给交互登录配的 `RemoteCommand` / `RequestTTY` 不影响（ccr 用 `-T -o RemoteCommand=none` 覆盖）
- 登录 shell 不限：远端脚本经 `sh -s` 从 stdin 执行，fish / csh 也没问题；会话目录是符号链接也行
- agent 日志在默认位置，或显式配置额外 Codex 日志目录

## 注意

- **计价口径**：花费按 CC Switch 的 `model-pricing.json` 折算 Token 成本，不等同于中转站账单。
  定价表里没有的模型记 `n/a`（CC Switch 看板里也是 0）。
- **归属**：CC Switch 把所有会话解析出的用量归到 `_codex_session` / `_session` 这类伪供应商，
  不区分是哪台服务器。要看按服务器拆分用 `ccr report`。
- **请求数口径**：Claude Code 把一次响应的每个内容块（thinking / text / tool_use）各写一行，
  `ccr report` 按 `message.id` 去重（Codex 按 `response_id`），与看板一致。
- **磁盘**：镜像保留 7 天未更新的文件（`ccr config --set-retain-days`）；仍在进行的 Codex 子代理会话
  的父会话链会一并保留（CC Switch 导入子会话需要它）；`config.json` 写坏时本轮不清理。
  `report --days` 超过保留期时更早的数据可能不全，报告里会提示。首次全量可能较大（如 3 天 41 个 Codex rollout ≈ 228MB 压缩后）。
- 服务器时钟与本地时区不一致时，"今日"归属按会话里记录的时间戳换算成本地时间。
