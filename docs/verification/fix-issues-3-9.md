# Issue #3–#9 修复验收（2026-10-29）

PR：[TekesMemory #10](https://github.com/TekesApps/TekesMemory/pull/10)。
基线 `75dda718b12f91880836927e9cb08eaee322086a`，最终受测代码提交
`74ffb7d29d84813d53d66d954b460bc429636309`。此后的提交仅整理验收文档。
平台 macOS arm64，Rust 1.95.0；所有数据库、凭据、服务和 hook 使用隔离 fixture。

## 最终结果

| 检查 | 观察结果 |
|---|---|
| `cargo test --locked --all-targets`（默认并行） | 83 通过：process 14、Store 57、transport_contract 12；2 个 SDK 测试默认 ignored，另行运行 |
| 官方 TypeScript SDK 1.30.0 | HTTP / stdio 2 通过：initialize、list、save、get、search、close |
| `cargo clippy --locked --all-targets -- -D warnings` | 通过 |
| `cargo fmt --all -- --check`、`git diff --check` | 通过 |
| cargo-audit 0.22.2，258 个锁定依赖 | 0 漏洞、0 unsound；另有 1 条 `yoke-derive 0.8.3` yanked 提示，未忽略漏洞公告 |

lib/bin/examples 的独立单元用例数均为 0，不将编译成功计为单元覆盖。
[命令结果摘要](fix-issues-3-9/results.txt)、[完整审计 JSON](fix-issues-3-9/audit.json)、
[锁文件差异](fix-issues-3-9/dependency-delta.json)、[rustls features](fix-issues-3-9/rustls-features.txt)
已随源码保存。原始日志位于 `/tmp/tekesmemory-fix-issues-evidence/`，可能被系统清理。

## 按原 Issue 验收

| Issue | 实现与证据 | 结果 |
|---|---|---|
| [#3](https://github.com/TekesApps/TekesMemory/issues/3) | 真实 `tool.completed → turn.settled → forget` 后 search/get 不再返回复制正文，重启仍成立；显式 A→B 的已处理/待处理顺序、A→B→C typed/raw alias 闭包、失效/重放、版本/FTS/payload/jobs、跨 scope 均覆盖。正常来源修订可用新 digest 重采集；不反向删除无关上游。已失效来源在接收事务内传播删除，覆盖子记录先到达或已处理的顺序。手工记录更换来源后仍按历史 keyed-source 清理。 | 通过 |
| [#4](https://github.com/TekesApps/TekesMemory/issues/4) | 脱敏限定内容字段；凭据正文继续脱敏，owner/workspace/thread、来源、事件及幂等身份不变。已知其他 scope 的 ID/revision 不能 correct/forget。 | 通过 |
| [#5](https://github.com/TekesApps/TekesMemory/issues/5) | 9007 字节正文可返回预算内 excerpt，保留规定字段/来源/scope，get 完整。默认 adapter 长多字节摘要按最终包装预算输出；超大元数据明确 truncated，能放入新页的命中保留 cursor，翻页有进展。 | 通过 |
| [#6](https://github.com/TekesApps/TekesMemory/issues/6) | 英文/中文超过 256 token 的尾词在 save/correct 后可检索；真实截断旧索引 fixture 重开后完整重建；查询仍提前限制为 256 个不同 token。 | 通过 |
| [#7](https://github.com/TekesApps/TekesMemory/issues/7) | 原 64×4096 emoji 来源记录经 HTTP/stdio 保存后 get，响应超过 2 MiB 仍完整。请求 2 MiB / 响应 8 MiB 分开有界；共享派生构造器在入队前检查最终读取封装。覆盖控制字符、4096 lineage、超限 save/correct 回滚，以及 schema 合法但派生响应超限时 records/observations/jobs/operations 全无残留。 | 通过 |
| [#8](https://github.com/TekesApps/TekesMemory/issues/8) | 两种传输覆盖 2^53−1、2^53、2^53+1、i64 两端、字符串、普通整数和通知；大 ID 初始化也原样返回。超范围数字返回 -32600 / id:null，stdio 可继续调用；紧接 EOF 时也先输出协议错误。 | 通过 |
| [#9](https://github.com/TekesApps/TekesMemory/issues/9) | rustls 0.23.46、rustls-webpki 0.103.16、webpki-root-certs/roots 1.0.11、Tokio 1.54.1。相关已知公告命中消除；原测试全部保留且最终通过，官方 SDK 两传输握手/调用/关闭通过。无新增 Brotli 包/features；loopback/no_proxy/no_redirect/no_retry 配置及拒绝非本机 endpoint 的回归保留。 | 通过 |

证据入口：[Store 用例](../../tests/store.rs)、[真实进程与 adapter 用例](../../tests/process.rs)、
[传输契约用例](../../tests/transport_contract.rs)。设计与契约见
[批次设计](../design/fix-issues-3-9.md)、[来源闭包](../design/lineage-forget.md)、
[检索预算与索引](../design/search-budget-and-index.md)、[运行契约](../contracts.md)。

## 审计与复现

RustSec 快照：`40f5be6fb12ed93fc591bf4ab23b7d793b3f2239`（最终核对未变）。
Cargo.lock SHA256：`8f501c18b18c3b71dba0fa4637ca6f168a457f5c6c3f9fe012dfea134f3b1138`。
审计之后锁文件未再变化，故复用该完整扫描结果。
最终测试二进制 SHA256：`02944492612a697b27b585f38c1347568f964b4491a78f151f272607f15d6195`。

```sh
cargo test --locked --all-targets
cargo clippy --locked --all-targets -- -D warnings
cargo fmt --all -- --check
git diff --check
MCP_SDK_ROOT=/absolute/node_modules/@modelcontextprotocol/sdk \
  cargo test --locked --test process official_typescript_sdk -- --ignored --nocapture
cargo audit --file Cargo.lock
cargo tree --locked -e features -i rustls
```

审阅发现的 raw alias 遗漏、来源修订误封禁、手工历史来源闭包遗漏、接收失效传播遗漏和
stdio 错误后立即 EOF 丢响应均先保留失败证据，再修复并加入最终回归。
部分委派并行测试曾在并发编译期间遇到 900ms 初始化超时；单独/串行重跑通过，
原因未独立确定。最终集成版本在默认并行下全部通过，未放宽超时或删除原断言。

## 升级限制与未验证范围

旧数据库中仍保留显式来源键的 completed payload，可在失效时恢复被旧实现覆盖的关联；
已用重建旧 record sources/dependencies 的 fixture 验证。旧 adapter 从未持久化的隐式聚合
宿主身份无法安全推断，不宣称全库自动修复。新服务与新 adapter 必须配套使用，
已有数据保持保留；若需处理这些历史孤立聚合，应另行凭原配置/日志验证或明确列单处置。

本批不发布新版本、不启用生产 launchd/hook、不访问生产数据库；没有修改 Kernel 或其他
消费仓库。没有新增真实 Kernel worker、Linux 实机、生产部署或长期负载验收结论。
仓库未配置测试 CI；本页记录的是上述本机验证，不把 Dependency Graph 当作测试 CI。
