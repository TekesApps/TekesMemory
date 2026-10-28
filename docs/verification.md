# Issue #3–#9 修复验收（2026-10-29）

最终集成验证与逐项证据见 [修复验收记录](verification/fix-issues-3-9.md)：83 项集成测试、
2 项官方 SDK HTTP/stdio 测试、Clippy 和格式检查通过；升级限制与审计提示在该记录中明确列出。

# rmcp 迁移验证（2026-10-13）

平台：macOS arm64；`rustc 1.95.0 (59807616e 2026-04-14)`；官方 `rmcp =3.5.1`，发布来源和 features 见 [实施文档](design/rmcp-migration.md)。所有结果针对最后源码，包括 SDK recovery、SSE retry 和 reqwest 自身 retry 均禁用的版本。

迁移前基线为 40 项集成测试通过、2 项 SDK 默认 ignored。最终结果如下，均为 exit 0：

| 验证类别 | 结果与边界 |
|---|---|
| 单元测试 | lib/bin/examples 均为 0 项；没有独立单元用例，不将编译成功记作单元覆盖 |
| 集成测试 | **50 项通过**：process 10、Store 契约 30、新增 transport_contract 10；SDK 两项默认 ignored 后另行运行 |
| 官方 SDK | TypeScript SDK **1.30.0**，HTTP、stdio **2 项通过**：initialize/list/save/get/search/close |
| 真实 Kernel MCP | 当前 Kernel 源码的真实客户端，HTTP、stdio 均通过 Legacy 2025-11-25 握手/发现/保存/读取/关闭；stdio 写入经 HTTP 读取，relay 退出后服务仍可读写 |
| 真实 Kernel worker | `production_turn_with_real_tekesmemory_service` **1 项通过**；真实 worker、hook 进程、Memory 服务，Provider 使用本地 fixture，验证上下文进入 worker 发出的请求及 observation 持久化 |
| 生命周期手动检查 | 隔离配置、活 HTTP 会话下 SIGTERM/SIGINT 均 exit 0（约 0.001s），同一数据目录重启取锁成功；stdio 在 owner 离线时空 EOF 仍 exit 0（约 0.006s） |
| 静态检查 | `cargo clippy --locked --all-targets -- -D warnings`、`cargo fmt --all -- --check`、`git diff --check` 均通过 |

测试覆盖 JSON 响应适配、协商回退/非法版本 header、空 clientInfo 兼容、非法 clientInfo、重复键、principal 会话隔离、热撤销工具和 scope、业务错误后继续调用、重启会话失效而 effect receipt 保留、HTTP/stdio 共用 Store、owner 离线后的 ping、stdin 保持打开时非法/超大输入有界退出、过期会话/超时写请求只发送一次、总超时包含握手。另有 mock 延迟 DELETE 响应 30ms 的回归：Client Drop 返回前已收到一次 DELETE 并完成响应确认，等待有界；避免短进程先退出而丢失会话清理。

立即断开 TCP 可能早于 dispatch：回执允许 `not_found` 或 `confirmed`，分别对应 0 或 1 条记录，测试不发送第二次写。另一项测试读取响应前缀后丢弃结果，证明已提交的 effect 可查询为 confirmed；随后**显式**同键重放返回原结果且不新增记录。不得把这些显式测试调用理解为客户端自动重试。

## 本次复现命令

在 TekesMemory 执行：

```sh
cargo test --locked --all-targets
cargo clippy --locked --all-targets -- -D warnings
cargo fmt --all -- --check
cargo build --locked --bins --example kernel_fixture
MCP_SDK_ROOT=/tmp/tekesmemory-rmcp-sdk/node_modules/@modelcontextprotocol/sdk \
  cargo test --locked --test process official_typescript_sdk -- --ignored --nocapture
TEKES_KERNEL_ROOT=/Users/frankqin/.codex/worktrees/9083/TekesKernel \
TEKES_MEMORY_BIN=/Users/frankqin/dev/TekesMemory/target/debug/tekes-memory \
  sh tests/kernel_mcp_interop.sh
```

SDK 安装在独立 `/tmp/tekesmemory-rmcp-sdk`，使用 `@modelcontextprotocol/sdk@1.30.0`。外部 Kernel MCP harness 脚本只生成临时 Cargo 项目，通过 path dependency 使用现有 Kernel crates；不写 Kernel 源码或锁文件。其独立解析依赖的 Cargo.lock 和 tree 留在证据目录，不能把 harness 依赖图等同于 Kernel workspace 锁文件。

在现有 Kernel checkout 执行：

```sh
CARGO_TARGET_DIR=/tmp/tekesmemory-rmcp-kernel-target \
TEKES_MEMORY_BIN=/Users/frankqin/dev/TekesMemory/target/debug/tekes-memory \
TEKES_MEMORY_FIXTURE_BIN=/Users/frankqin/dev/TekesMemory/target/debug/examples/kernel_fixture \
  cargo test --locked -p tekes-worker production_turn_with_real_tekesmemory_service -- --ignored --nocapture
```

## 来源与证据

- Memory 基线 HEAD：`a2bca6a9d342f873982dba061e5b5f664c9e0651`，加本次未提交修改。
- Kernel：`e1e9d8b877ed312c464c5a6e1e89f7854ad24e35`，测试前后 clean；mcp/schema crate 0.4.6。
- 最终测试二进制 SHA256：`198c3dfbbd404d77c7c55e17e6886dd647703d896a667c6659d1d299a89317c3`。
- 日志：`/tmp/tekesmemory-rmcp-{all-tests,clippy,fmt,diff-check,sdk,kernel,kernel-mcp}-final.log`。
- 生命周期日志：`/tmp/tekesmemory-rmcp-lifecycle-final.log`；原 CLI `init --port 0` 拒绝行为未改，测试使用正常 init 后仅将隔离配置的 port 改为 0。该手动 smoke 使用前一二进制 `68560ef70325acccd4910c12cd67f65615c5a501e64957c79681801401b3c675`；其后仅补同步 Client Drop 有界关闭确认，serve/stdio 退出路径未改。最终二进制通过上述完整自动测试及互操作。
- 完整命令汇总：`/tmp/tekesmemory-rmcp-validation-final.md`。
- 外部 harness 源码：[Rust](../tests/support/kernel_mcp_interop.rs)、[运行脚本](../tests/kernel_mcp_interop.sh)。证据目录 `/tmp/tekesmemory-rmcp-kernel-mcp-evidence` 保存 Cargo.lock、依赖树、HEAD/status 和 binary hash。

所有服务数据和凭据均使用隔离临时目录；迁移验收阶段没有操作生产数据、修改其他仓库、提交、推送、发布或启用 launchd；之后用户授权 0.2.1 源码发布，见 [发布说明](releases/v0.2.1.md)。原 Store、数据库 schema、业务 schemas、配置和 adapter 代码未改动。

未测：真实模型 Provider、生产/日常 Kernel 部署、Linux 实机、DSH 自动生命周期；一小时会话老化与 1024 会话满载未做长时压力验收。CLI/进程和隔离互操作通过不替代这些验收。原始日志在 `/tmp`，可能被系统清理；本页保留验收摘要与复现命令。

---

# Rust 0.2.0 验证记录

日期：2026-09-16。平台：macOS arm64；Rust 1.95.0。

以下均针对 Rust 实现。旧 Python 结果已移入 [历史归档](../archive/python-prototype/docs/verification.md)，不作为 Rust 验收证据。

## 已完成

| 验证 | 结果与边界 |
|---|---|
| Rust 自动测试 | 39 项通过：29 项存储/契约、10 项真实进程测试；2 项外部 SDK 测试默认 ignored，另行显式执行通过 |
| Clippy | `--all-targets -- -D warnings` 通过 |
| 官方 MCP SDK | TypeScript SDK 1.30.0；HTTP 与 stdio 两条链路完成 initialize/list/save/get/search/close |
| Kernel 集成 | 真实 Kernel worker 回合调用独立 Rust Memory 服务；参考文本进入 Provider 请求，工具与 settle observation 持久化，推理内容未采集 |
| release 安装包 | `cargo install --path . --locked` 安装至隔离目录；Mach-O arm64 原生二进制 |
| 安装后运行 | 源码目录外、空环境执行 init/serve；HTTP 保存、真实 kernel hook、launchd plist 生成通过 |
| 检索基准 | 10,000 episode + 5,000 fact，40 次热查询；release 数据库路径 p50 17.015ms、p95 19.906ms；不含 HTTP、进程启动或模型 |

存储测试覆盖：重启与幂等、跨 scope/operation 隔离、model 无权伪造 observation、中文检索预算、TTL/pin、并发 revision 竞争、历史版本、时间区间冲突、删除正文/索引/版本、来源修订与失效、删除防重放、事务中途/COMMIT 失败回滚、后台五次失败停止、游标签名和 generation、procedure 仅作为数据、未来数据库版本拒绝。

进程测试覆盖：HTTP auth/Host/Origin/media、会话绑定和关闭、凭据热撤销、单写锁、五个真实 Rust hook 进程、无 user item 的续轮检索、来源日志边界/符号链接/伪造记录拒绝、宿主日志不改写、服务重启、stdio relay。

## 本地复现

在 TekesMemory 仓库运行：

```sh
cargo test --locked
cargo clippy --locked --all-targets -- -D warnings
cargo build --locked --bins --examples
cargo install --path . --locked --root /absolute/isolated-install
cargo run --example deployment_smoke -- /absolute/isolated-install/bin/tekes-memory
cargo run --release --example benchmark
```

官方 SDK 测试，Node.js 仅为测试客户端，服务运行不依赖它：

```sh
npm install --prefix /tmp/tekesmemory-sdk --no-audit --no-fund @modelcontextprotocol/sdk@1.30.0
MCP_SDK_ROOT=/tmp/tekesmemory-sdk/node_modules/@modelcontextprotocol/sdk \
  cargo test --test process official_typescript_sdk -- --ignored --nocapture
```

Kernel 集成在 TekesKernel 仓库运行，使用前一步构建的 Rust 二进制：

```sh
TEKES_MEMORY_BIN=/absolute/TekesMemory/target/debug/tekes-memory \
TEKES_MEMORY_FIXTURE_BIN=/absolute/TekesMemory/target/debug/examples/kernel_fixture \
  cargo test -p tekes-worker production_turn_with_real_tekesmemory_service -- --ignored --nocapture
```

`kernel_fixture` 是仅供测试的 Rust example，不属于安装后的 CLI。它启动真正的 Memory 服务，准备隔离配置，并在 Kernel 回合后查询持久化 observation。

## 本次证据日志

- `/tmp/tekesmemory-rust-final-tests.log`
- `/tmp/tekesmemory-rust-final-clippy.log`
- `/tmp/tekesmemory-rust-sdk-final.log`
- `/tmp/tekesmemory-rust-kernel-final.log`
- `/tmp/tekesmemory-rust-install-final.log`
- `/tmp/tekesmemory-rust-deployment-final.log`
- `/tmp/tekesmemory-rust-benchmark.log`

## 尚未验收

未启用真实用户的 launchd 或项目 hook，未重启/部署日常 Kernel；没有 Linux 实机、远程服务、DSH 生命周期扩展、模型自动提炼或长期跨会话记忆质量的验收结论。基准仅测当前 fixture 的热数据库查询。
