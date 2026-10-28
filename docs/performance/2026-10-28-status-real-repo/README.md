# GT_DemoB：Status 页真实大仓库刷新测量

2026-10-28，Windows 本机，基于 gitui `56fdad6e` 的生产代码。此次只增加测量工具和报告，**没有改动生产扫描实现，也没有替换 Scoop 程序**。

结论：有优化空间，优先减少重复扫描。当前文件列表约 **0.56 秒**更新，等行数统计等后台任务全部完成约 **1.16 秒**。绘制耗时不到 1 毫秒。复用 libgit2 Repository 的确认实验约快 **9%**；干净状态下跳过空栏行数统计，预计可以省去约 **0.6 秒的后续后台工作**，但后者尚未实现，不是已交付的提速。

## 仓库与测试边界

- 实际扫描 `D:\work\ue_prj\GT_DemoB`，30,568 个受跟踪文件，索引 4,422,301 字节，pack 合计 36,502,410,293 字节（36.50 GB / 34.00 GiB）。pack 大小不等于每次刷新读取的数据量。
- 测试时仓库干净：暂存、工作区、未跟踪结果均为零。起始检查未发现运行中的 UnrealEditor 或 gitui。
- Ryzen 7 9700X，8 核 16 线程；D 盘为 ZHITAI TiPlus7100 2TB NVMe。Rust 1.99.1；Release、LTO、单 codegen unit；git2 0.21.0 / libgit2 1.9.7。Status 扫描仍走 libgit2，未调用 gix。
- 扫描真实工作区、对象库与 Git 配置；将索引复制到 gitui 的 `target/` 下，通过子进程 `GIT_INDEX_FILE` 重定向。Rust 在扫描前验证实际索引路径。保留生产的 `update_index(true)` 行为，但刷新只写副本。
- 两轮实验均核对原 HEAD、原索引 SHA256 与 mtime、仓库 config SHA256、完整 porcelain 状态，前后完全一致。未创建项目文件、未修改 Git 配置、未启动编辑器。
- **热缓存、当前干净状态测试**，不是冷启动、海量修改、冲突或大批重命名测试；没有为测速改动项目资源。所有模式都验证结果为零。
- `status.showUntrackedFiles` 未配置，GitUI 默认 All。UI 测量用测试环境默认界面选项，没有装载用户全局界面偏好。

## 实际 Status 组件刷新

调用真实 `Status::show/update → AsyncStatusPair → update_git → draw`，并接收行数统计、分支比较等实际后台通知。绘制到 160×50 的 Ratatui TestBackend。共 23 轮，前 2 轮丢弃，保留 21 轮；p95 为这些单次刷新样本的 nearest-rank 第 95 百分位。

| 测量范围 | 中位数 | p95 |
| --- | ---: | ---: |
| 触发刷新 → Status 结果已处理并绘制 | 561.13 ms | 655.08 ms |
| 触发刷新 → 所有 Status 后台任务结束、通知处理完 | 1,156.13 ms | 1,276.71 ms |
| 发起更新的同步调用 | 1.11 ms | 1.35 ms |
| 一轮所有通知处理函数累计时间 | 0.0046 ms | 0.0082 ms |
| 一轮所有绘制累计时间（稳态为 2 次） | 0.556 ms | 0.643 ms |

这里的“已绘制”指组件写入内存终端，不包括终端宿主显示、输入事件循环、轮询等待；不是屏幕录像测得的端到端延迟。后台扫描不会让 UI 线程同步阻塞 0.56 秒，文件列表也不用等行数统计全部完成后才更新。

默认轮询间隔是 5 秒（`src/main.rs`）；从外部文件变化到下一次触发还可能等待 0–5 秒。`--watcher` 使用 2 秒 debounce（`src/watcher.rs`）。本表从**触发刷新**起计时，不能把扫描速度与这段触发等待混为一谈。本次没有改动更新模式。

## 扫描与统计拆分

第一轮实验采用 3 个顺序轮换/反转的 block，每种模式每个 block 9 次，前 2 次预热，共保留每模式 21 次。当前机器存在约 0.5–1 秒的扫描波动，保留全部样本，没有删除慢样本。不同模式不是同一条调用链的分段，不能把各行中位数直接相加。

| 生产 API / 对照 | 中位数 | p95 |
| --- | ---: | ---: |
| `get_status_split` 完整生产扫描 | 643.76 ms | 862.96 ms |
| `AsyncStatusPair` 发起到通知收到 | 711.07 ms | 974.56 ms |
| 状态后台任务 + 两项行数统计串行累计 | 1,178.99 ms | 1,790.77 ms |
| 仅 index 状态 | 62.47 ms | 64.93 ms |
| 仅 workdir 状态 | 535.08 ms | 887.38 ms |
| staged 增删行统计 | 70.41 ms | 101.09 ms |
| unstaged 增删行统计 | 553.09 ms | 984.31 ms |
| 原生 `git status --porcelain=v1 -z -uall`，包含启动进程 | 324.51 ms | 331.31 ms |

`refresh_pipeline` 是后台工作量代理，不是 UI 端到端结果。组件实测另见上表；两轮测量发生在不同时间，波动解释了生产扫描 644 ms 与组件结果 561 ms 的差别，不能把它解释为优化收益。

固定相同收集逻辑的 libgit2 options 对照：

| 模式 | 中位数 |
| --- | ---: |
| 原始 All / update_index / staged rename 开启 | 562.33 ms |
| 关闭 update_index | 569.72 ms |
| 未跟踪目录仅列目录（Normal） | 579.20 ms |
| 不检查未跟踪文件 | 565.78 ms |
| 关闭 staged rename 检测 | 568.14 ms |

这些变体没有测出稳定收益；当前没有新文件和重命名，因此也不能据此评价它们在相应繁忙场景下的成本。关闭未跟踪文件或重命名检测会改变展示语义，不建议作为默认提速方式。关闭 update_index 还可能让时间戳变化但内容相同的文件被反复读取、哈希。

原始扫描的仓库 open 调用约 0.72 ms，空结果收集约 0.0001 ms；主要耗时在 libgit2 status 内部。复用对象的收益不能归因于省掉这 0.72 ms，而在跨轮保留索引、对象等缓存以及减少重建/释放工作。

## 复用 Repository 的确认实验

相同 options 和收集逻辑，ABBA 紧邻交错，前 4 次预热丢弃，保留各 21 次。复用对象仅用于实验，不是生产代码改动；soft index refresh 和 update_index 仍开启。

| 模式 | 中位数 | p95 |
| --- | ---: | ---: |
| 每次新开 Repository | 557.38 ms | 679.87 ms |
| 复用 Repository | 508.41 ms | 627.97 ms |

按中位数下降 **8.79%**，约 **49 ms**；21 对紧邻样本中 17 对更快，配对节省时间中位数 50.43 ms。这是值得考虑的小幅改进，不能宣称数量级提升或完全稳定的 9%。复用对象的初次加载在计时外，因此仅代表连续刷新。

额外对照：原生 Git 在相同索引副本上，`core.preloadIndex=true` 中位数 320.36 ms，`false` 390.68 ms，各 21 次 ABBA 样本。Git 的该配置会并行比较索引与文件系统，默认开启，见 [Git 官方配置文档](https://git-scm.com/docs/git-config#Documentation/git-config.txt-corepreloadIndex)。该实验表明并行文件检查有价值，但不能把 Git 与 libgit2 的全部差距都归因于这一项。给仓库设置该配置也不等于实现了 libgit2 内部并行化。

## 建议的优化顺序

1. **空栏跳过增删行统计。** `src/tabs/status.rs::update_status` 在 Changed 和 Unchanged 两种结果下都调用 `request_line_stats`；`asyncgit/src/linestats.rs` 又执行 staged、unstaged 两次整仓 diff。干净仓库可以由最新状态快照确定两栏为空，直接把计数设为零，减少约 0.6 秒后续后台工作；文件列表本身仍需完成原有扫描。实现时须把扫描代次/统计范围纳入任务有效性判断，避免旧任务覆盖新结果。**不能直接在所有 Unchanged 情况下跳过**：同一个文件持续显示 Modified 时，内容和行数仍可改变。
2. **在单个后台扫描器内复用 Repository。** 本次测得约 9% 扫描收益。保留索引 soft refresh，处理外部 git add/reset、HEAD/配置切换、索引替换、linked worktree 和错误恢复。不要用 `no_refresh(true)` 绕过正确性要求。
3. **更大改动再考虑增量/并行检查。** 当前成本集中在 workdir 遍历。利用文件事件缩小扫描范围，或引入其他后端，可以继续研究，但需覆盖漏事件、忽略规则变化、目录变更和全量校验。本轮数据不足以支持直接替换后端。

已存在的优化无需重复做：两栏合并一次扫描、扫描请求合并、不变状态跳过树重建。本轮扫描小于默认 5 秒轮询间隔，未证明需要为这个干净仓库调整重扫调度。大量变更时的树重建和终端输出成本尚未测量。

## 复现与数据

在 gitui 根目录构建（按本机 OpenSSL 构建环境设置变量，依赖不变）：

```powershell
cargo build --release --locked -p asyncgit --example perf_status_real --features vendor-openssl
cargo test --release --locked --bin gitui --no-run --message-format json
python scripts/perf_status_real.py D:/work/ue_prj/GT_DemoB target/status-perf-new
python scripts/perf_status_confirm.py D:/work/ue_prj/GT_DemoB target/status-confirm-new --ui-binary <上一条cargo输出的gitui测试executable路径>
```

输出目录必须在被测仓库外，且是新目录。工具要求干净的普通仓库；当前脚本不是任意 linked worktree 的通用基准。确认脚本只运行指定 ignored 测量，不会运行其他测试或自动修改项目文件。不要直接在真实索引上运行 Rust 示例。

- [环境、版本和验证记录](environment.json)
- [主实验概要](scan/summary.json)、[所有主实验样本（含预热）](scan/samples.csv)、[前后仓库核对](scan/environment.json)
- [确认实验概要](confirmation/summary.json)、[保留样本](confirmation/samples.csv)、[组件原始输出](confirmation/ui.txt)、[前后仓库核对](confirmation/environment.json)
- `scan/` 和 `confirmation/` 中保留全部原始数值输出；不提交索引副本、项目文件、Git 配置内容或可执行文件。

验证：Release 测量程序与组件测试构建通过；新增示例 Clippy `-D warnings` 通过；Status 现有测试 14 项通过；真实仓库 ignored probe 单独运行通过。没有生产行为修改，因此未重新安装 GitUI。

常规 `cargo clippy --workspace --locked` 通过。额外扩大到 `--tests` 的 Clippy 检查被原有测试代码中的 580 条 lint 诊断阻断（主要为测试里的 unwrap）；新增组件测量模块的诊断已清理。未把这项扩大检查标为通过，也未修改无关测试。
