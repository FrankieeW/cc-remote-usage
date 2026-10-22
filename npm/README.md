# cc-remote-usage

把跑在服务器上的 **Codex / Claude Code agent 用量**同步进 [CC Switch](https://github.com/farion1231/cc-switch)
的「用量」看板 —— 自动发现 SSH 主机、交互选择要同步哪几台、增量拉取会话日志。

Rust 编写的单文件二进制，npm 包只是分发壳（安装时按平台自动下载对应二进制）。

## 安装

```bash
npm i -g cc-remote-usage
ccr --version
```

支持 macOS（arm64/x64）与 Linux（arm64/x64）。安装时会从
[GitHub Releases](https://github.com/FrankieeW/cc-remote-usage/releases) 下载对应平台的二进制；
网络受限时可手动下载后放到包内 `vendor/<platform>-<arch>/ccr` 并 `chmod +x`。

> 需要本机已安装并配置好 CC Switch（`cc-switch` 在 PATH 里），且支持
> `cc-switch sessions sync-usage --app <codex|claude>`（实测 5.11.3）。
> 先确认 CLI 与桌面版兼容，再用 `ccr config --pin-cc-switch` 锁定 CLI 版本。

## 用法

```bash
ccr hosts --probe          # 发现并探测可同步的服务器
ccr sync                   # 交互多选 → 增量同步 → 导入 CC Switch
ccr sync --all --quiet     # 全自动（定时任务用）
ccr import netcup          # 只重试本地镜像导入，不连接服务器
ccr report --days 7        # 按服务器/模型汇总 Token 与花费
ccr status                 # 同步状态、失败次数、下次重试时间
ccr daemon install         # 装 launchd 定时任务（每 15 分钟）
```

完整的参数说明见 [README](https://github.com/FrankieeW/cc-remote-usage#用法)。

Pharos 等编排器使用独立 `CODEX_HOME` 时，需要显式添加日志目录，默认不会自动搜索：

```bash
ccr config --add-codex-root 'server=/srv/pharos/runtime/projects'
ccr sync server
```

首次完整回补，之后增量同步；只传输 `sessions` / `archived_sessions` 下的 JSONL。
同步或导入只要有主机失败，就返回非零退出码，`--quiet` 同样适用。

## 环境变量

| 变量 | 用途 |
|---|---|
| `CCR_BINARY_PATH` | 覆盖要执行的二进制路径 |
| `CCR_LOCAL_BINARY` | 安装时直接用本地二进制，不下载（离线/开发用） |

## 许可

MIT
