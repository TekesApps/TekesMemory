# TekesMemory — Rust

独立本地 MCP 记忆服务及 TekesKernel 生命周期扩展。**正式实现全部为 Rust**，不依赖 Python，不依赖 Kernel crate。

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
