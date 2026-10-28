# 修复批次：Issue #3–#9

基线：`75dda718b12f91880836927e9cb08eaee322086a`（0.2.1）。本批复用既有
[Issues](https://github.com/TekesApps/TekesMemory/issues)，验收条件以各 Issue 原文为准。

## 范围与依赖

| Issue | 交付 | 验收重点 |
|---|---|---|
| [#3](https://github.com/TekesApps/TekesMemory/issues/3) | adapter 原始行依赖、Store 派生闭包与待处理任务清理 | processed/pending、失效、重启、重放、历史版本、FTS、跨 scope；旧数据恢复边界单独说明 |
| [#4](https://github.com/TekesApps/TekesMemory/issues/4) | 内容脱敏不改变身份 | owner/workspace/thread、来源及幂等身份；已知其他 scope 的 ID/revision 不能修改或删除 |
| [#5](https://github.com/TekesApps/TekesMemory/issues/5) | 预算内的紧凑搜索结果 | 长正文仍可发现、保留来源与 scope、完整 get、明确截断与分页、adapter 最终包装 |
| [#6](https://github.com/TekesApps/TekesMemory/issues/6) | 查询和完整文档索引分离 | 英文/中文尾词、save/correct、已有索引重建、查询限制 |
| [#7](https://github.com/TekesApps/TekesMemory/issues/7) | 请求/响应分别有界 | 合法大记录 HTTP/stdio 可读、提交前拒绝不支持数据、多字节与封装边界 |
| [#8](https://github.com/TekesApps/TekesMemory/issues/8) | JSON-RPC 使用保留整数精度的编码 | HTTP/stdio 大整数与有符号边界、字符串、通知、无效 ID 协议错误 |
| [#9](https://github.com/TekesApps/TekesMemory/issues/9) | 更新已知受影响锁定依赖 | 全依赖审计、原测试、HTTP/stdio 握手/调用/关闭、features 与本机连接约束 |

#3/#4、#5/#6、#7/#8 分别在隔离工作树实施，主代理串行集成共享文件。
#3 的来源上限与 #7 的响应边界共同复核；#5 的 adapter 搜索包装与 #3 的 observation
构造分段集成。#9 只更新相关锁定依赖，最终测试使用集成后的源码和锁文件。

## 验收原则

新回归、原测试、静态检查与独立官方 SDK HTTP/stdio 冒烟覆盖最终集成版本。
数据库、配置、凭据与 hook fixture 全部隔离；不操作生产数据、不启用 launchd，
不修改 Kernel 或其他消费仓库。不能从静态编译推断真实设备或生产验收。

旧数据缺失的来源信息不能凭空恢复；相关迁移必须以真实旧库结构验证，明确
可恢复和不可恢复范围，后者不能用新数据通过替代。阻碍回填到对应 Issue，
其余工作继续推进。只有原验收条件全部有证据支持的 Issue 才能关闭。

最终实现、命令、结果和未验证项将回填到各 Issue 与批次验收记录。
