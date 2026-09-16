# 当前运行契约（Rust 0.2.0）

[项目入口](../README.md) · [运维](operations.md) · [验证](verification.md)

## 协议与身份

MCP 兼容基线为 `2025-11-25`。HTTP `/mcp` 返回 JSON，GET 返回 405（不提供主动 SSE）；支持 DELETE 结束会话。所有请求校验 Host、Origin 和 Bearer，握手后的会话绑定当前 principal。配置中的 role、tools 和 scopes 是服务授权依据，调用参数不能扩权。

`memory.observe` 仅 adapter 可见且可调用；默认 model 可使用另六个工具。工具 inputSchema 与 [schemas](../schemas/) 一致，拒绝未知字段。返回 `structuredContent` 和兼容文本，业务失败为 isError 和结构化 code。没有实现 OAuth 发现流程，凭据通过本机私有配置部署；这不宣称远程 OAuth MCP 服务兼容。

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

事实字段 `subject/predicate/value/environment/valid_from/valid_to` 用于区分冲突和适用区间。重叠适用范围、不同值时标为 contested；不按“谁更新得晚”选真值。显式纠正使冲突值一致后解除标记。程序字段包含前置条件、步骤、成功判据、验证环境和失败模式，不执行这些步骤。

时间采用 UTC Unix 秒。verified_at 默认 null；导入时间不冒充验证时间。episode 默认 90 天有效，pin 豁免自动到期；查询立即过滤过期数据。维护任务随后移除过期正文/版本/索引，保留来源身份。显式删除可以删除已过期记录，pin 不阻止显式删除。

workspace 查询可合并被授权的 user scope；thread 查询可合并被授权的 workspace/user scope。默认 init 只授权一个 workspace，不自动启用跨项目用户记忆。scope 必须完整匹配结构，不支持仅凭路径推测 scope。

## 幂等与任务

写操作以 principal + scope + idempotency_key 去重，并校验请求 digest。相同键的原返回值保持稳定；查询 get/status 才反映当前状态。相同来源身份、序号、类型和 digest 的重复交付不生成新 observation；相同源 digest 携带不同正文被拒绝。

后台任务每次在一个事务中整理一条情景，因此进程崩溃会提交全部或回滚全部，无跨进程任务租约。处理失败最多重试五次，指数间隔后成为 failed。turn_opened 和 source_invalidated 不生成多余情景正文。没有模型提炼器、自动候选晋升或技能安装器。

数据库 schema 版本为 1，更高版本拒绝写入。配置文件为 0600，数据目录为 0700。SQLite FULL 同步与单进程 flock 保护写入，FTS5 secure-delete 配合 SQLite secure_delete；[SQLite 的说明](https://www.sqlite.org/fts5.html#the_secure_delete_configuration_option)解释了两者的不同覆盖范围。服务不承诺擦除 APFS 快照、磁盘历史或外部备份。

## 来源与删除边界

Kernel 扩展只读配置允许的 thread_root，规范化系统路径别名后按目录描述符访问，拒绝最终文件符号链接与越界。只读给定 seq 前缀，校验 genesis workspace/thread，最多读取 128MiB，单记录最多 2MiB。内联可见 text 被提取，reasoning/thinking、sealed 内容和 asset 正文不读取。

压缩捕获记录 covers 中的可见文本；大文本有摘要长度上限，不等于完整日志备份。服务不自行打开 ledger_path。扩展按已读可观察材料计算来源摘要，来源引用保留 line/seq。宿主日志重写和删除没有自动监听器，需要受信任扩展显式发送 source_invalidated；调用示例见运维文档。

Kernel 成功回执可重放过去的 context；Memory 删除不会修改这个回执、宿主原始日志或实际请求资产。服务 TTL 与删除后重放防护也无法撤回此前已发送的内容。

## 预算与兼容边界

请求体至多 2MiB，查询最多 100 条，返回参考数据至多 32KiB。当前估算器为 UTF-8 字节上界，故 estimated_tokens=1500 可能明显少于 1500 个模型 token；不是准确 tokenizer。Kernel 扩展再检查最终包装文本上限。

常驻服务最多 32 个并发 MCP 请求、1024 个会话，每个会话最长一小时；短扩展调用主动关闭会话。只支持本机 loopback，不提供远程部署、浏览器跨域通配、TLS 或跨机器同步。DSH 等客户端可用显式 MCP 工具，自动生命周期适配尚需单独实现与验证。


## Rust 实现

正式运行入口是 `tekes-memory` 原生二进制；服务、客户端、stdio relay、配置生成器和
Kernel format-2 扩展均为 Rust，不调用 Python。SQLite 由 rusqlite 的 bundled 功能编译，
不依赖系统 Python 或系统 SQLite。Cargo.lock 固定依赖版本。

沿用 schema_version=1 和 SQLite user_version=1；保留既有表、操作幂等键与来源身份语义。
配置 JSON 和工具参数不变。传输输入严格拒绝重复键与非有限数值；工具整数参数由 schema 单独校验。
