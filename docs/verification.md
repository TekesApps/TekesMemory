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
