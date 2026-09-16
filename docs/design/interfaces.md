# TekesMemory 接口设计

> 实现前的设计基线。当前交付状态与实际配置请看 [项目入口](../../README.md) 和 [运行契约](../contracts.md)。
[总览](README.md) · [宿主接入](integration.md) · [实施验收](delivery.md)

状态：提案。以下工具名、字段和返回值尚未实现；实施时应生成版本化 JSON Schema、正反例及兼容测试。

## 1. 标准范围

服务使用 MCP 的 `initialize`、`tools/list`、`tools/call`。`memory.search` 等名称及其参数由 TekesMemory 定义；标准传输不代表记忆语义已被行业标准化。宿主 hook JSONL 也不是 MCP JSON-RPC。

首版选择本地常驻 Streamable HTTP MCP 服务，绑定 loopback，由安装/服务管理组件拥有启停和单实例锁。模型客户端和短生命周期 hook 扩展复用同一服务，扩展作为普通 MCP client 完成协商与调用。

本地连接仍要求受控凭据、Origin 检查及授权 scope；仅绑定 localhost 不等于拥有调用者身份。日志不记录 bearer 值。后续如需 stdio，提供独立兼容入口；不能让每个 hook 启动一个各自拥有后台队列的数据库服务。

这些机制对应官方 [tools](https://modelcontextprotocol.io/specification/2025-11-25/server/tools) 和 [transports](https://modelcontextprotocol.io/specification/2025-11-25/basic/transports)；部署默认和 Memory 数据模型是本项目选择。

## 2. 工具集合

所有请求含 `schema_version: 1`。读写返回 `schema_version` 和 `request_id`；写操作另含稳定 `operation_id`。未知字段拒绝，新增可选字段也应同步更新版本兼容测试。

| 工具 | 主要输入 | 主要输出 | 调用者 |
|---|---|---|---|
| `memory.search` | scope、query、kinds、budget、cursor? | items、conflicts、next_cursor、truncated | 模型/扩展，只读 |
| `memory.get` | scope、id、revision? | 完整记录、来源和版本 | 模型/扩展，只读 |
| `memory.save` | scope、kind、content、sources、idempotency_key | id、revision、status | 获准显式保存的模型客户端 |
| `memory.correct` | scope、id、expected_revision、replacement、sources、idempotency_key | 新 revision、替代关系 | 获准纠正的客户端 |
| `memory.forget` | scope、ids、expected_revisions、idempotency_key | 检索隐藏状态、物理清理状态、宿主副本提示 | 获准删除的客户端 |
| `memory.observe` | scope、source_event、observation、idempotency_key | accepted、operation_id、processing_state | 宿主扩展专用 |
| `memory.status` | operation_id | accepted/processing/completed/failed、结构化错误 | 原调用者或管理客户端 |

`memory.observe` 可以使用标准 MCP tools/call，但不得在不具备扩展身份的模型连接上列出或允许调用。权限由服务执行，tool annotations 仅作提示。模型请求保存不能自称为可信宿主事件。

### Scope

```json
{"kind":"workspace","owner_id":"local-user","workspace_id":"ws-example"}
```

`user` scope 不带 workspace；`thread` scope 另带 `thread_id`。授权连接决定可用 owner 和 workspace；不匹配返回 `scope_denied`，不泄漏目标是否存在。

### 检索示例

下面是 MCP `tools/call.params`，不是 Kernel hook 配置：

```json
{
  "name": "memory.search",
  "arguments": {
    "schema_version": 1,
    "scope": {"kind":"workspace","owner_id":"local-user","workspace_id":"ws-example"},
    "query": "项目如何运行针对性测试",
    "kinds": ["episode","fact","procedure"],
    "budget": {"max_items":5,"max_utf8_bytes":12000,"estimated_tokens":1500}
  }
}
```

搜索输出的每项至少包含 `id / revision / kind / content / status / sources / verified_at / expires_at`。预算对最终序列化参考材料计费，包含引用和状态标签。过期、已删除、无权限数据不得出现在结果、数量统计或错误详情中。

## 3. Observation：业务语义与宿主事件分离

`observation.type` 只接受 `turn_opened / tool_result / context_checkpoint / turn_outcome / source_invalidated`。这些是 Memory 的输入分类；Kernel 和其他宿主的原生事件名保存在 source 元数据中。

`source_event` 至少包含：

```text
host                 宿主命名空间，例如 tekeskernel
host_instance_id     安装实例身份，重启保持稳定
workspace_id         宿主 workspace 身份
thread_id            宿主 thread 身份
line_id              同一 thread 内日志线身份
event_id             本次宿主交付的稳定身份
source_seq           原始记录序号；checkpoint 使用 through_seq
source_digest        原始可观察源版本的摘要
```

每种 observation 的必要 payload：

| type | payload |
|---|---|
| turn_opened | turn_id、公开任务描述、来源引用 |
| tool_result | turn_id、调用身份、工具名、最终结果状态、脱敏摘要、来源引用 |
| context_checkpoint | turn_id、through_seq、covers、manual、可观察内容摘要/引用 |
| turn_outcome | turn_id、宿主 outcome、reason/classification、可见最终答复、证据引用 |
| source_invalidated | 源身份、旧 digest、失效范围、原因 |

扩展解析宿主格式、读取授权来源并构造这些字段；服务不解析 Kernel 原始 ledger，不接受“读取任意路径”指令。引用是证据定位信息，不是服务读取宿主磁盘的默认权限。

对不存在的字段返回 unknown 或省略可选字段；不同宿主的 completed/stop 语义必须保留区别。模型的自述和宿主可观察状态分别记录。

## 4. 幂等与并发

幂等键限定在调用者和授权 scope 内。服务将键、请求规范化 digest、结果与写入数据放入同一事务：

- 相同键、相同 digest：返回原提交结果。
- 相同键、不同 digest：`idempotency_conflict`，不能覆盖。
- 不同交付键、同一源身份/序号/digest：复用 observation，避免换 hook 配置后重复导入。
- 同源身份/序号但不同 digest：标记来源修订，先让依赖旧源的内容失效，再生成新版本。

Kernel event_id 绑定冻结 binding，所以不宜作为唯一的业务去重键。显式 save 使用调用者生成的稳定键；correct/forget 必须满足 expected_revision，冲突返回 `revision_conflict`。

后台任务只在持久提交之后可执行，重启恢复过期任务租约。入队与 observation 不能两阶段分开提交，否则 accepted 后可能永远没有处理任务。

## 5. 返回值和失败

工具结果使用 `structuredContent`，同时给出简短文本表示。业务失败用 `isError: true` 和稳定错误码；协商、未知方法和协议解码失败按 MCP 协议错误处理。

错误码建议：`invalid_argument / scope_denied / not_found / revision_conflict / idempotency_conflict / budget_exceeded / source_unavailable / unavailable / unsupported_schema_version`。错误不包含凭据或原始敏感正文。

连接中断或超时意味着提交结果未知；使用同一个幂等键重试或查询 status。取消不保证已经提交的写入回滚。hook 在获得 accepted 后才回复成功；否则以失败退出，让宿主保留重试机会。

## 6. 版本边界

独立管理 MCP 协议版本、Memory schema_version、Kernel hook format，以及数据库 migration 版本。适配器只依赖公开 wire 数据，不直接打开 Memory 数据库或链接 Kernel 私有类型。

每次不兼容字段/语义变更提升 Memory schema_version；服务显式拒绝不支持版本。数据库升级前备份、事务迁移，旧服务遇到更高版本拒绝写入；客户端版本协商不能代替数据库回退设计。
