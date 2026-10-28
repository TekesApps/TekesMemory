# 当前运行契约（Rust 0.2.0）

[项目入口](../README.md) · [运维](operations.md) · [验证](verification.md)

## 协议与身份

协议由精确锁定的官方 `rmcp 3.5.1` 实现，服务仅开放 `2025-11-25`。HTTP `/mcp` 返回 JSON，GET 返回 405（不提供主动 SSE）；支持 DELETE 结束会话。所有请求校验 Host、Origin 和 Bearer，握手后的会话绑定当前 principal。配置中的 role、tools 和 scopes 是服务授权依据，调用参数不能扩权。

`memory.observe` 仅 adapter 可见且可调用；默认 model 可使用另六个业务工具及 `memory.reconcile_effect`；后者对具备任一写工具权限的 principal 可见。工具 inputSchema 与 [schemas](../schemas/) 一致，拒绝未知字段。返回 `structuredContent` 和兼容文本，业务失败为 isError 和结构化 code。没有实现 OAuth 发现流程，凭据通过本机私有配置部署；这不宣称远程 OAuth MCP 服务兼容。

SDK 有状态传输的单个终结 SSE 响应由薄适配转为既有 JSON 响应；没有 SSE 重连重放或主动通知。HTTP 与 stdio 均兼容旧的空 clientInfo 对象，自动补实现名称/版本；标准客户端应提供真实 name/version。MCP envelope 类型错误由 SDK 拒绝，工具参数对象的业务 schema 错误保留 isError 结果。`memory.reconcile_effect` 现在也完整校验既有 schema，未知字段返回 `invalid_argument`。

传输按 [MCP transports](https://modelcontextprotocol.io/specification/2025-11-25/basic/transports) 实现 JSON 响应模式；工具发现和调用遵循 [MCP tools](https://modelcontextprotocol.io/specification/2025-11-25/server/tools)。记忆工具名与字段是本项目定义。

## 数据与工具

| 工具 | 实际行为 |
|---|---|
| search | 权限过滤、有效期过滤、词法匹配、相关度排序；items 与 conflicts 分开；有签名游标 |
| get | 精确 scope 内读取当前或指定 revision；已删除/过期记录不可读 |
| save | 显式写入 active episode/fact/procedure；fact/procedure 必须提供对应结构；可设置 pin 和 expires_at |
| correct | expected_revision 条件更新，保留版本及历次来源依赖；事实内容或方法结构必须一起更新 |
| forget | 原子隐藏/清除正文、版本、索引、关联 observation 和依赖闭包；保留最小防重放标记 |
| observe | 原子持久接收与入队，不在 HTTP 请求中做模型工作；同源修订先清除旧派生数据 |
| status | 仅原 principal 可查询，重新校验其当前 scope 权限 |
| reconcile_effect | 按 principal + effect key 查询持久写入回执，重新校验当前 scope 权限；未发现回执不等于可以自动重放 |

事实字段 `subject/predicate/value/environment/valid_from/valid_to` 用于区分冲突和适用区间。重叠适用范围、不同值时标为 contested；不按“谁更新得晚”选真值。显式纠正使冲突值一致后解除标记。程序字段包含前置条件、步骤、成功判据、验证环境和失败模式，不执行这些步骤。

时间采用 UTC Unix 秒。verified_at 默认 null；导入时间不冒充验证时间。episode 默认 90 天有效，pin 豁免自动到期；查询立即过滤过期数据。维护任务随后移除过期正文/版本/索引，保留来源身份。显式删除可以删除已过期记录，pin 不阻止显式删除。

workspace 查询可合并被授权的 user scope；thread 查询可合并被授权的 workspace/user scope。默认 init 只授权一个 workspace，不自动启用跨项目用户记忆。scope 必须完整匹配结构，不支持仅凭路径推测 scope。

## 幂等与任务

`tools/call._meta["io.tekes/idempotencyKey"]` 是外部 effect key，与业务参数 `idempotency_key` 各有用途。effect receipt 和业务修改在同一 Store 事务提交；返回 `confirmed` 时携带原工具结果，缺失时返回 `not_found`。MCP session/task 不代替业务回执，网络超时或断连不证明写入回滚；客户端不得因此自动重放写操作。

写操作以 principal + scope + idempotency_key 去重，并校验请求 digest。相同键的原返回值保持稳定；查询 get/status 才反映当前状态。相同来源身份、序号、类型和 digest 的重复交付不生成新 observation；相同源 digest 携带不同正文被拒绝。

脱敏只处理 save 的 content/fact/procedure、correct 的 replacement/fact/procedure，以及
observation 的 summary/reason。scope、记录 ID、来源引用、事件身份与幂等身份保持原值，
数据库操作使用通过鉴权的同一作用域。

后台任务每次在一个事务中整理一条情景，因此进程崩溃会提交全部或回滚全部，无跨进程任务租约。处理失败最多重试五次，指数间隔后成为 failed。turn_opened 和 source_invalidated 不生成多余情景正文。没有模型提炼器、自动候选晋升或技能安装器。

数据库 schema 版本为 1，更高版本拒绝写入。配置文件为 0600，数据目录为 0700。SQLite FULL 同步与单进程 flock 保护写入，FTS5 secure-delete 配合 SQLite secure_delete；[SQLite 的说明](https://www.sqlite.org/fts5.html#the_secure_delete_configuration_option)解释了两者的不同覆盖范围。服务不承诺擦除 APFS 快照、磁盘历史或外部备份。

## 来源与删除边界

Kernel 扩展只读配置允许的 thread_root，规范化系统路径别名后按目录描述符访问，拒绝最终文件符号链接与越界。只读给定 seq 前缀，校验 genesis workspace/thread，最多读取 128MiB，单记录最多 2MiB。内联可见 text 被提取，reasoning/thinking、sealed 内容和 asset 正文不读取。

压缩捕获记录 covers 中的可见文本；大文本有摘要长度上限，不等于完整日志备份。服务不自行打开 ledger_path。扩展按已读可观察材料计算来源摘要，来源引用保留 line/seq。宿主日志重写和删除没有自动监听器，需要受信任扩展显式发送 source_invalidated；调用示例见运维文档。

Kernel 成功回执可重放过去的 context；Memory 删除不会修改这个回执、宿主原始日志或实际请求资产。服务 TTL 与删除后重放防护也无法撤回此前已发送的内容。

## 搜索索引与结果预算

文档索引覆盖完整合法正文；查询分词最多保留 256 个不同 token。索引规则变更时，服务按 `search_index_version` 在现有数据库上重建 FTS 内容，不改数据库或记录 schema。搜索项保留 id、revision、kind、content、status、sources、verified_at 和 expires_at；当正文超出搜索预算时返回 query-centered excerpt 并标记 `content_truncated`。response `truncated` 表示正文有省略或仍有更多匹配项。`budget.utf8_bytes` 按最终序列化 `items` 与 `conflicts` 计费；完整记录始终可按 ID 用 `memory.get` 读取。

## 预算与兼容边界

请求体至多 2MiB，响应体至多 8MiB（独立于请求上限），查询最多 100 条，返回参考数据至多 32KiB。读取响应采用独立的 8MiB 解析界限；超过界限不会作为完整成功响应交付。写入 Store 前检查工具参数预算；记录落盘前检查完整 get 响应封装的大小，后台 observation 入队前也检查生成记录的大小，因此超限返回 `budget_exceeded` 且不会提交不可读记录。JSON-RPC ID 支持有符号 64 位整数和字符串，超出范围的数字 ID 返回 `-32600` 请求错误；传输 JSON 使用保留整数精度的普通 JSON 序列化，JCS 仍仅用于规范化摘要等用途。当前估算器为 UTF-8 字节上界，故 estimated_tokens=1500 可能明显少于 1500 个模型 token；不是准确 tokenizer。Kernel 扩展再检查最终包装文本上限。

常驻服务最多 32 个并发 MCP 请求、1024 个会话，每个会话最长一小时；短扩展调用主动关闭会话。只支持本机 loopback，不提供远程部署、浏览器跨域通配、TLS 或跨机器同步。DSH 等客户端可用显式 MCP 工具，自动生命周期适配尚需单独实现与验证。


## Rust 实现

正式运行入口是 `tekes-memory` 原生二进制；服务、客户端、stdio relay、配置生成器和
Kernel format-2 扩展均为 Rust，不调用 Python。SQLite 由 rusqlite 的 bundled 功能编译，
不依赖系统 Python 或系统 SQLite。Cargo.lock 固定依赖版本。

沿用 schema_version=1 和 SQLite user_version=1；保留既有表、操作幂等键与来源身份语义。
配置 JSON 和工具参数不变。传输输入严格拒绝重复键与非有限数值；工具整数参数由 schema 单独校验。
