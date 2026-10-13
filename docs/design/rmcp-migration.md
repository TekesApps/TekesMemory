# 官方 rmcp 协议层迁移

日期：2026-10-13。迁移前基线：`a2bca6a9d342f873982dba061e5b5f664c9e0651`。开始时工作区干净，仓库及上级未发现 AGENTS.md。迁移实施阶段未提交、推送、发布或更改其他仓库；随后用户授权以 0.2.1 发布，发布范围见 [发布说明](../releases/v0.2.1.md)。

## 决策与实施顺序

直接使用官方 [Rust SDK](https://github.com/modelcontextprotocol/rust-sdk) 的发布 crate，不新建通用 MCP SDK 或 Runtime。只读核对现实现、运行契约、测试和 Kernel 接口后，先记录本计划，再实现与验证。

选型为 `rmcp = "=3.5.1"`（crates.io 发布版本；不跟踪 main），关闭默认 features。SDK 发布声明最低 Rust 1.88，项目继续固定 Rust 1.95.0；直接启用 features：`server`、`client`、`transport-io`、`transport-streamable-http-server`、`transport-streamable-http-client-reqwest`。以 Cargo.lock 固定传递依赖；1.88 是 SDK 自身声明，不是整个依赖图经过验证的最低工具链。项目验证工具链为 1.95.0。

所锁 [rmcp 3.5.1](https://docs.rs/crate/rmcp/3.5.1) 的 `ProtocolVersion::KNOWN_VERSIONS` 包含 2024-11-05、2025-03-26、2025-06-18、2025-11-25、2026-07-28；TekesMemory 本次仅声明和开放 `2025-11-25`，不因此开启新版无会话生命周期或 tasks。最终实际兼容范围见完成记录。不能以 SDK main 的能力说明替代所锁发布版本的实测。

## 原行为、差异与兼容措施

| 范围 | 迁移前行为 | 实施要求及差异 |
|---|---|---|
| HTTP | 手写 JSON-RPC、会话和分发；`/mcp` JSON 响应；GET 405、DELETE 关闭 | 用 rmcp Streamable HTTP 和 ServerHandler；保留 JSON 响应与路径。明确记录 SDK 引入的握手/错误校验差异 |
| 协商 | 固定返回 `2025-11-25`，后续 header 仅允许该值 | 维持宿主兼容基线；标准协商交由 SDK，实测版本回退和非法版本 |
| 会话 | principal 绑定、最长一小时、至多 1024；32 并发请求 | SDK 管理协议会话，薄层保留身份隔离、时限及资源限制；session 不是持久业务身份 |
| 身份授权 | 每请求校验 Host/Origin/Bearer 并重读配置 | 不在握手后冻结权限；工具列表和调用均使用当前 principal、role、tools 和 scopes |
| 工具 | 七项业务工具及按写权限展示的 `memory.reconcile_effect` | 名称、schema、工具元数据、动态可见性和业务调用不变；修正文档漏列 reconcile 工具 |
| 校验与结果 | 严格 JSON 重复键/大小校验；Store schema 校验；structuredContent + text | 保留业务 schema 和结果；SDK 校验协议 envelope；未知工具是协议错误，业务失败仍是 isError + structured error code |
| stdio | 手写换行协议、合成 initialize，单个 HTTP session relay | rmcp 管理 stdio 协议；只转发到同一个权威 HTTP 服务，不打开数据库 |
| 客户端 | 同步 reqwest，单次 deadline，无重试 | SDK 客户端薄同步适配；显式关闭会话恢复/重放，维持总超时预算和私有凭据/no proxy/no redirects |
| 副作用 | 业务 idempotency_key 和 `_meta.io.tekes/idempotencyKey` effect receipts | 不改 Store 事务、digest、principal/scope隔离；超时/断连结果未知时只显式查询回执，不自动重发操作 |
| 生命周期 | serve 唯一 Store owner，flock、维护线程、ready file、SIGINT/SIGTERM | CLI 参数与 ready endpoint 合同不变；stdio EOF 不停止权威服务；健康检查采用握手/ping，没有新增独立 health URL |

授权边界属于 TekesMemory；MCP session/task 不替代 SQLite 业务幂等、后台任务和 effect receipts。SDK 的传输重连、取消或请求超时不能成为自动重新执行写入的理由。

## 明确的兼容适配

rmcp 3.5.1 的 `json_response` 仅在无会话路径直接提供 JSON；本服务需要保留 principal 绑定会话。因此使用 SDK 的会话传输，再把单次请求对应的唯一 terminal SSE response 转为既有 JSON HTTP response。此层不自行执行 JSON-RPC 方法或业务工具，也不提供 SSE replay。若将来增加服务端主动通知，必须重新评估该适配，不能静默丢弃通知。

原实现允许 `initialize.params.clientInfo={}`。HTTP 与 stdio 为兼容本地已有调用方，均补稳定的 name/version；标准客户端仍应发送真实实现身份。协议 envelope 的无效类型由 SDK 拒绝，工具对象内的业务 schema 错误继续返回 `isError`。未经 initialized 的调用不属于有效客户端序列；旧 stdio 曾宽松接受该序列，现在必须先发送 `notifications/initialized`。`clientInfo` 非对象等非法 envelope 由 SDK 拒绝。`memory.reconcile_effect` 原实现遗漏了完整参数 schema 校验，现拒绝未知字段；合法参数和回执结果不变。

stdio 的空输入仍正常退出，收到首个初始化消息后才连接权威 HTTP 服务；凭据不进入 argv 或日志。严格输入门限保留 2MiB 和重复键检查，普通 OS stdin reader 避免 SDK 结束时等待不可取消的 Tokio stdin read。ping 转发给 HTTP owner，不能在 owner 离线时本地报告成功。

HTTP 会话 principal/绝对一小时期限/配额属于安全薄层；SDK manager 内部会话与其同步清理。并发初始化预留配额并串行核对 SDK 实际会话数，避免仅限制外部映射；断开的未绑定握手会话限时清理。SDK DELETE 的成功响应适配为原有 HTTP 200。客户端明确设置 `reinit_on_expired_session(false)`、无 SSE 重试策略及 reqwest `retry::never()`，单次预算包含握手。同步 Client 析构时先关闭命令队列，再在剩余预算内最多等待 100ms 的 SDK DELETE 完成确认，避免短 hook 进程先退出而来不及关闭会话；不无界 join，不增加请求预算。

## 实施和验收

1. `src/server.rs`：SDK HTTP 服务与工具 handler；保留认证、严格输入及资源限额薄适配。
2. `src/client.rs`、stdio 入口：SDK 协议/适用传输；保留唯一权威服务、凭据与超时边界。
3. 原 Store 和数据库 schema 不因协议迁移改变。
4. 增加 HTTP/stdio、协商、动态授权、协议/业务错误、断连和 effect receipts 风险测试。
5. 单元测试、进程集成、官方 SDK 与真实 Kernel 互操作分别记录通过/失败/未测。真实 Kernel 只读使用现有代码与隔离目录/凭据，不接入生产数据。
6. 记录具体 SDK 校验导致的兼容差异与可用的客户端修正；不能修改断言来掩盖授权或业务回归。

## 完成记录

迁移验收时，代码修改保留在工作区，尚未提交、推送或发布；后续 0.2.1 发布是单独授权的交付阶段。50 项集成测试、2 项官方 SDK 测试、真实 Kernel HTTP/stdio MCP 客户端与 1 项 worker 集成均通过；Clippy 和格式检查通过。lib/bin 无独立单元用例（0 项），不把集成测试重新归类为单元测试。完整命令、版本来源、二进制哈希和未测边界见 [本次验证记录](../verification.md)。

Store、数据库/工具 schemas、配置和 adapter 未改动；保留现有生命周期及唯一服务写锁。真实模型、日常部署及 Linux 实机不在本次通过结论内。
