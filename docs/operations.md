# 安装、配置和运维

[入口](../README.md) · [运行契约](contracts.md)

## 安装与启停

使用 `cargo build --release --locked` 构建，再把 `target/release/tekes-memory` 安装到固定版本目录。生成配置时记录当前 Rust 二进制的绝对路径；替换同路径二进制不会被 Kernel 的配置冻结机制检测到，应使用版本目录。

`init --directory ... --workspace ... --thread-root ...` 生成：

```text
service.json                 服务地址、数据目录、保留期限、principal/grants
model-credential.json        模型客户端私有 token 文件
adapter-credential.json      宿主扩展私有 token 文件
adapter-<digest>.json         固定版本扩展配置
hooks/*.json                 五个 Kernel format-2 绑定模板
```

service.json 的 principals 存 token_sha256，原 token 只在对应 credential 文件。model 默认没有 observe 权限。删除 principal 或变更 hash 即撤销旧 token，服务每次请求重读身份与授权；host/port/data_directory 的更改需要重启。更换 token 文件时保持 0600 并用原子替换。

手动运行 `serve --config /absolute/service.json`，SIGTERM/SIGINT 正常结束，后台未处理 observation 留在数据库。`--ready-file /absolute/new-file.json` 原子发布实际 endpoint，主要用于测试和进程管理。已有 ready-file 不覆盖。

生成 macOS 用户服务配置：

```sh
tekes-memory launchd-plist --config /absolute/service.json --output /absolute/local.tekesmemory.plist
```

该命令只生成 plist。正式启用由使用者执行 `launchctl bootstrap gui/$(id -u) /absolute/local.tekesmemory.plist`；停用为 `launchctl bootout gui/$(id -u) /absolute/local.tekesmemory.plist`。同一数据目录由 flock 阻止第二个服务写入。此实现回合没有启用真实用户的 launchd 服务或项目 hook。

## 显式调用工具

把工具 arguments 存为 JSON 文件，然后运行：

```sh
tekes-memory call --endpoint http://127.0.0.1:43187/mcp \
  --credential-file /absolute/model-credential.json \
  --tool memory.save --arguments-file /absolute/save.json
```

save.json 示例，owner_id 应使用 service.json 的 principal owner_id，workspace_id 使用真实绑定：

```json
{"schema_version":1,"scope":{"kind":"workspace","owner_id":"local-501","workspace_id":"demo"},"kind":"fact","content":"项目使用 cargo test 运行测试","fact":{"subject":"project","predicate":"test_runner","value":"cargo test"},"sources":[{"host":"user","ref":"explicit-request","digest":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"}],"idempotency_key":"save-test-runner-1"}
```

摘要字段应替换为实际证据摘要，示例中的 a 仅展示格式。后续 correct/forget 使用 get 返回的 revision。批量删除要求 ids 与 expected_revisions 一一对应，任何一项失败都不部分删除。

source_invalidated 使用 adapter 凭据调用 memory.observe。payload 至少包含 turn_id、summary、sources、source_keys；source_keys 来自先前 observe 返回值或检索记录的 sources。source_event 提供这次失效操作自己的稳定宿主身份和序号。删除旧源同时清理包含旧源的历史版本与派生记录，采用保守的依赖闭包。

不要读取/打印 credential 文件正文调试。MCP 调用故障只输出稳定错误码，扩展 stderr 不打印输入材料。服务的模式匹配脱敏是额外过滤，不能识别所有业务秘密；接入方仍负责只提交获准保存的可见材料。

## 升级、备份和清理

1. 停止服务；备份整个 data 目录与私有配置，保持原权限并限制备份访问。
2. 在新版本目录安装服务，核对 SQLite 与 schema 版本；更高数据库版本会被旧程序拒绝。
3. 用备份副本验证启动、读取、检索后切换服务入口。0.2.0 没有跨 schema 自动降级；回退要恢复相匹配的数据备份。
4. 重新生成或版本固定扩展配置与 hook argv。Kernel 换 binding 集合有新 baseline，不能假设旧失败通知会自动转移；需要明确补录。

forget 提交立即隐藏数据并移除服务正文/索引；返回的 physical_cleanup=checkpoint_pending 表示 WAL 截断由维护线程随后执行。这个原响应是提交时状态，不是磁盘擦除证明。操作记录只保留请求 digest 与无正文结果，防止旧请求重放恢复数据。

卸载时先停用各宿主 hook、停止服务，再删除程序；是否保留 data 由用户决定。Memory 的 forget 不清理宿主日志、hook 回执、Provider 请求资产或外部备份。恢复旧备份可能恢复旧数据，必须重应用删除记录或采用删除后备份。

## 故障定位

| 症状 | 检查 |
|---|---|
| 启动拒绝 | Rust 二进制版本、bundled SQLite/FTS5、配置 0600、目录 0700、端口占用、单写锁 |
| HTTP 401/403 | 凭据是否撤销、Host/Origin 是否允许；不要放开 loopback 限制作为修复 |
| scope_denied | principal 的 owner/scopes/tools，模型不能调用 observe |
| invalid_argument | 对照已导出的 schema，包括未知字段和 revision 类型 |
| source_unavailable | 线程根、genesis 身份、来源前缀完整性、删除防重放标记 |
| accepted 后未检索到 | status 中任务是否完成、query 是否匹配、字节预算、TTL 与冲突状态 |
| Kernel 本轮无记忆 | hook 是否已捕获，服务是否在线；失败通知仅下次 worker 激活重试 |


## 从 Python 原型切换

原型归档在 `archive/python-prototype/`，不在 Cargo 发布包中。先停止旧服务，备份私有
配置与 data，再用 Rust 二进制对副本运行 `serve --config ...` 验证。SQLite 格式仍为 1，
Rust 可沿用该格式；无需重新导入。旧 hook argv 中的 Python 解释器和 run.py 必须换成
固定 Rust 二进制及 `kernel --config ...`，stdio/launchd 入口也须同步更新。
不要对已有配置目录重复运行 init；它会拒绝覆盖凭据。
