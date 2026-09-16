# 验证记录

日期：2026-09-16。结果适用于本次未提交工作树，真实用户项目没有启用记忆采集。

## 已取得的证据

| 验证 | 结果与范围 |
|---|---|
| Python 本地测试 | 35 项；包括事务回滚、重启、崩溃后读取、作用域、来源修订/删除、TTL、schema 和真实 HTTP/扩展进程 |
| 官方 MCP TypeScript SDK | 1.30.0；初始化、list、save、get、search、会话关闭通过 |
| Kernel 真实 Memory 接入 | 真实服务与扩展；四次实际发往本地模拟 Provider 的请求均包含记忆；工具最终结果与 settled observation 落盘 |
| Kernel worker 回归 | 114 通过，3 项条件跳过；其中真实 Memory 项另行显式运行 |
| 15,000 条记忆查询 | 10,000 情景、5,000 事实，40 次热查询；本机 p50 23.909ms、p95 25.286ms；纯查询，不含模型、网络/进程启动 |
| 安装包 | venv 中构建和安装 Python wheel；已安装包在源码目录之外、空环境中完成 init/serve/真实 hook/launchd 生成检查 |

不把上述结果解释为真实模型效果、DSH 生命周期、桌面安装或长期生产运行验收。没有对照实验支持 token 节省百分比。

## 复现命令

在 TekesMemory 目录：

```sh
python3 -m unittest discover -s tests -v
python3 tests/benchmark.py
python3 tests/package_smoke.py --python /absolute/venv/bin/python
```

官方 SDK 检查使用外部测试依赖，不是服务运行依赖：

```sh
MCP_SDK_ROOT=/absolute/node_modules/@modelcontextprotocol/sdk \
NODE=/absolute/node python3 tests/run_official.py
```

Kernel 的跨项目测试是显式选入项。在 TekesKernel 目录执行：

```sh
TEKES_MEMORY_ROOT=/absolute/TekesMemory \
TEKES_MEMORY_PYTHON=/absolute/python3 \
cargo test -p tekes-worker production_turn_with_real_tekesmemory_service --locked -- --ignored --nocapture

cargo test -p tekes-worker --bin tekes-worker --locked
```

所有进程测试使用临时配置、随机端口/凭据和临时数据目录。benchmark 的 fixture 批量导入用于隔离查询成本，不是逐笔持久化写入吞吐指标。

## 本次修复过的边界

- macOS `/var` 到 `/private/var` 路径别名导致合法线程前缀被拒绝。
- 增量 Provider 请求只有工具结果时，从当前回合已接纳输入恢复检索查询。
- 源记录修订发生在后台任务处理前时，取消旧任务并清除旧 payload。
- 历史 revision 的来源依赖保留，后续来源失效能够清除旧版本正文。
- ready-file 原子发布，避免调用者读到尚未写完的文件。
- 事实冲突查询按事实键索引分组，避免每次写入做全库两两比较。

## 仍待后续验收/实现

- 真实任务的记忆收益、错误事实使用率与提炼成本。
- 模型提炼、候选晋升和可执行技能导出。
- DSH 等其他宿主的自动生命周期适配。
- 宿主日志/回执删除与 Memory 删除的统一入口。
- 真实用户 launchd 安装、自启动和升级回退操作。
