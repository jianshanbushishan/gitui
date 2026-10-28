# Status 两项优化：GT_DemoB 前后对比

2026-10-28，基于 `2d8a26fe` 的优化前生产实现，对比加入“空栏跳过行数统计”和“后台扫描复用 libgit2 Repository”后的实现。

实测结果：**文件列表更新约快 10%，全部后台刷新工作完成约快 56%**。这是干净仓库的连续刷新结果；有实际修改时仍需计算对应栏的行数，不能直接套用 56%。

## 实测数据

同一台 Ryzen 7 9700X / NVMe Windows 主机，实际扫描 `D:\work\ue_prj\GT_DemoB` 的 30,568 个受跟踪文件。两版本均使用 Release、LTO、默认功能。3 组顺序为旧→新、新→旧、旧→新；每个进程 23 轮，前 2 轮预热丢弃，**各保留 63 次独立刷新样本**。测量期间不运行构建或测试负载。

| 指标 | 优化前中位数 | 优化后中位数 | 下降 | 优化前 p95 | 优化后 p95 |
| --- | ---: | ---: | ---: | ---: | ---: |
| 触发刷新到列表结果处理并绘制 | 551.50 ms | 495.36 ms | **10.18%** | 649.58 ms | 554.57 ms |
| 触发刷新到全部 Status 后台任务完成 | 1,126.20 ms | 495.36 ms | **56.02%** | 1,265.05 ms | 554.57 ms |
| 发起刷新的同步调用 | 1.110 ms | 1.077 ms | — | 1.426 ms | 1.448 ms |
| 每轮绘制累计时间 | 0.570 ms | 0.197 ms | — | 0.696 ms | 0.321 ms |

p95 使用 63 个单次刷新样本的 nearest-rank 百分位。绘制减少主要由于干净状态不再额外产生行数统计完成通知（稳态每轮 2 次绘制变成 1 次），不是渲染器自身获得相同比例的加速。

调用真实 `Status::show/update`、异步任务、通知处理和绘制逻辑，输出到 160×50 Ratatui TestBackend。**不包含终端宿主显示和默认 5 秒轮询等待**，也不是录像测得的屏幕端到端延迟。热缓存、干净工作区，无大批变更/冲突/重命名场景。

## 两项改变

1. **空栏统计立即归零，不启动全仓 diff。** 以最新成功状态快照为准；工作区只有未跟踪新文件时也不需要计算 tracked 增删行数。非空栏仍计算，同一文件持续 Modified 时依然更新行数。统计请求带范围和代次，旧任务不能覆盖新快照的零值；相同参数的重复轮询保留正在运行的结果，避免慢任务一直失效。
2. **AsyncStatusPair 跨刷新保留 Repository。** 保留 libgit2 的 soft index refresh 与 update_index；外部暂存或索引替换仍可被识别。每轮用新打开的轻量对象检查仓库位置、HEAD、配置及 Git 环境身份，改变时重建缓存，扫描错误后清除缓存。收益来自保留索引/对象缓存，不是完全取消每次打开仓库。

同步 `get_status_split` API 仍维持独立打开语义，所以本次以真实异步 Status 组件作为性能验收，而非直接调用同步 API 代替用户路径。两项优化一起测量，没有将总收益全部归因于某一项。

## 数据完整性与验证范围

- 使用 `GIT_INDEX_FILE` 指向 gitui `target/` 下的副本；真实工作区、对象库和配置保持原样读取。
- 前后核对原 HEAD、原索引 SHA256 与 mtime、仓库 config SHA256、完整 porcelain 状态，全部一致；项目状态始终干净。
- 基线 executable 的 SHA256 与上轮报告保存的 UI 基线完全一致。候选在最终统计调度修正后重建，保留二进制 hash 和源文件 hash。
- 原 probe 要求收到 LineStats 通知；新实现干净状态不会发该通知，候选 probe 移除此要求，仍等待 status 已处理、所有后台 pending 为零且通知队列为空。两版本“所有任务完成”的范围一致。
- 6 个测量进程均通过仓库空状态、两栏计数为零等断言。功能回归和最终工作区测试由实现验证单独执行；此报告不将性能测量等同于所有功能正确性的证明。

## 复现

先分别保存优化前后的 Release gitui 单元测试 executable（`cargo test --release --locked --bin gitui --no-run --message-format json` 的 `compiler-artifact.executable`），再运行：

```powershell
python scripts/perf_status_optimization.py D:/work/ue_prj/GT_DemoB target/status-opt-new <baseline.exe> <candidate.exe>
```

输出目录必须是仓库外的新目录；工具要求当前项目干净，不会创建项目资源或修改原索引。不要在真实索引上直接执行测量 probe。

- [汇总](summary.json)
- [全部样本，含预热](samples.csv)
- [环境、二进制/源文件指纹和仓库前后核对](environment.json)
- `0/1/2-baseline.txt`、`0/1/2-candidate.txt` 保存每组原始输出。

没有提交索引副本、可执行文件、项目资源或完整 Git 配置。

集成验证：默认与 Onig 配置各 551 项测试通过、0 失败、4 ignored；cargo clippy --workspace --locked 通过。
