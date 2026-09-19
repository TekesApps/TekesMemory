# TekesMemory — Rust

[![License: MIT](https://img.shields.io/badge/License-MIT-yellow.svg)](LICENSE)

> **English:** TekesMemory is a standalone local MCP memory service written in Rust (SQLite + FTS5, scoped, correctable, forgettable memory) plus lifecycle hooks for the TekesKernel agent host. It is a *vibe-coded* implementation of the 5-layer agent memory model described in the independently compiled playbook *"Agent Memory Architecture: 5 Layers That Cut Token Cost 90% and Make Your Agent Actually Learn"* (Sept 2026, not affiliated with Anthropic). See [Origin](#来源与致谢) below. MIT licensed.

独立本地 MCP 记忆服务及 TekesKernel 生命周期扩展。**正式实现全部为 Rust**，不依赖 Python，不依赖 Kernel crate。MIT 协议开源。

## 来源与致谢

本项目是对 **《Agent Memory Architecture — 5 Layers That Cut Token Cost 90% and Make Your Agent Actually Learn》**（"The 5-Layer Playbook"，2026 年 9 月独立整理的 13 页综述，作者声明不隶属于 Anthropic、Mem0、Snowflake 等任何机构）的一次 **vibe coding 实现**：设计文档和绝大部分代码由 AI 编码代理（Codex / Claude Code）在人工审阅下生成，人负责提需求、定边界、验收和取舍。

该 playbook 基于 CoALA 的分类，把 agent 记忆分为五层：工作记忆（上下文窗口）、情景记忆（发生了什么）、语义记忆（什么是真的）、程序记忆（怎么做）、遗忘（该删什么）。TekesMemory 的落地方式：

| Playbook 中的层 | TekesMemory 的做法 |
|---|---|
| 工作记忆 | 不接管上下文窗口；只在 `context.prepare` 返回带来源的参考文本，由宿主装配 |
| 情景记忆 | `memory.observe` + 5 个 Kernel hooks 记录任务开启、工具结果、压缩前材料与结束状态 |
| 语义记忆 | `memory.save/get/search` 保存带作用域、时间与证据的断言；`memory.correct` 版本化修正，冲突显式标记 |
| 程序记忆 | 方法描述作为带版本的记忆项保存；技能安装与执行留给宿主 |
| 遗忘 | `memory.forget`、TTL、来源失效、删除防复活；后台任务与写入同事务落盘 |

与原文的取舍（详见 [设计基线 §2](docs/design/README.md)）：

- 保留：来源引用、作用域隔离（user / workspace / thread）、跨会话检索、事实修正、版本与遗忘。
- 调整：冲突不一律"保留最新"，新时间戳不等于更可靠；"成功三次"只产生方法候选，不自动晋升或执行技能；历史记忆不覆盖实时工具结果、当前用户要求和宿主权限。
- 推迟：知识图谱、向量检索、多级摘要、记忆驱动任务分配、跨机器同步。

Playbook 中引用的节省 token / 提升准确率等数字未在本项目复现，不作为承诺。PDF 原文版权归其编者，仓库不附带；文中的提示词、代码和配置是分析对象，不是本项目的执行指令。

## 能力

- 7 个 MCP 工具：`memory.search/get/save/correct/forget/observe/status`。
- Loopback Streamable HTTP MCP、可选原生 stdio relay。
- bundled SQLite + FTS5，中文双字/英文词法检索、作用域隔离、版本修正、冲突、TTL 和来源失效。
- observation 与后台任务同事务落盘；幂等重放、删除防复活、重启恢复。
- 5 个 Kernel format-2 hooks；独立 Rust 扩展进程，按宿主规则超时降级。
- 私有凭据、model/adapter 分权、热撤销、单服务写锁、launchd plist 生成。

当前后台仅做确定性情景整理。自动模型提炼、自动事实/技能晋升、向量检索、DSH 生命周期扩展、跨机器同步未实现。

## 构建与安装

macOS / Linux，Rust **1.95.0**（rust-toolchain.toml 固定），需要 C 编译器来构建 bundled SQLite。

```sh
cargo build --release --locked
cargo test --locked
cargo install --path . --locked --root /absolute/tekesmemory-v0.2.0
/absolute/tekesmemory-v0.2.0/bin/tekes-memory --help
```

Cargo.lock 固定依赖；生成 hook 前应先把二进制安装到固定版本目录。不要让生产 hook 指向会被开发构建覆盖的 target/debug。

## 隔离演示

```sh
mkdir -p /tmp/tekesmemory-demo/threads
tekes-memory init --directory /tmp/tekesmemory-demo/config \
  --workspace demo --thread-root /tmp/tekesmemory-demo/threads
tekes-memory serve --config /tmp/tekesmemory-demo/config/service.json
```

默认 endpoint 为 `http://127.0.0.1:43187/mcp`，可通过 `init --port` 更换。
init 只生成配置和模板，不自动启用真实项目 hooks。

stdio 客户端配置使用：

```sh
tekes-memory stdio --endpoint http://127.0.0.1:43187/mcp \
  --credential-file /tmp/tekesmemory-demo/config/model-credential.json
```

stdio 转发到已运行服务，不另开数据库写入者。凭据值不放入命令行。

## Kernel 接入

init 输出的 `hook_templates` 中有五个 canonical JSON 文件，安装到项目 `.agents/hooks/` 后由 Kernel 下一次捕获配置。service.json 属于 Memory 服务，不是 Kernel 配置。

| Hook | 行为 |
|---|---|
| turn.before | 记录任务打开 observation |
| context.prepare | 检索并返回有来源的参考文本 |
| tool.completed | 记录最终工具 outcome 与可见文本 |
| context.before_compact | 捕获 covers 中的可观察材料 |
| turn.settled | 记录实际结束状态 |

默认 hook 超时 1500ms，扩展网络总预算 1000ms，包含初始化。
检索材料不授予权限，不执行记忆中的程序步骤。

## 文档

- [运行契约](docs/contracts.md)
- [安装、配置、升级与运维](docs/operations.md)
- [Rust 验证与复现](docs/verification.md)
- [设计基线](docs/design/README.md)
- [JSON schemas](schemas/)
- [历史 Python 原型归档](archive/README.md)

旧 Kernel note/recall 数据不会自动导入。删除服务数据不会删除宿主日志、hook 回执、已发送的 Provider 请求或备份。

## 许可

[MIT](LICENSE)。
