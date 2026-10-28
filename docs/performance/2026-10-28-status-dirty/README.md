# GT_DemoB 隔离副本：非干净 Status 刷新性能

2026-10-28。对比两项优化之前的 `2d8a26fe` 与已安装版本对应的 `a535e39c`：空栏跳过行数统计、跨刷新复用 libgit2 Repository。生产代码没有新增改动；本轮扩展测量 probe，使它支持非空状态、精确计数校验和 Git/App 两类异步通知。

结论：**三种场景的列表更新中位耗时均减少约 11%；全部后台任务的收益取决于是否仍需扫描工作区统计行数。** 未暂存约减少 12%，只有已暂存约减少 51%，只有未跟踪约减少 57%。未暂存的总体 p95 变差，不能声称所有尾延迟都改善。

## 场景与隔离

- GT_DemoB 当前 30,568 个受跟踪文件，共 37,990,105,881 字节（37.99 GB），全部独立复制到 gitui 的 `target/status-dirty/replica/fixture`。**没有硬链接、目录联接或指向原工作区的写路径**。
- 副本有独立 `.git`、索引、refs、config 和对象写入目录，通过普通 alternates 文本文件只读访问原对象库。复制 tracked `.gitignore`、`.gitattributes` 及 `.git/info/exclude`；不复制忽略的 UE 生成物和原 hooks。
- 文件复制及副本索引 stat 缓存刷新在计时前完成。不是原路径的直接脏化，也不等同于包含全部 ignored 生成目录的真实工作区，因此请在同一副本的两版本间比较，不与上一轮原路径绝对值机械相减。
- 只在副本中依次构造三种状态，其他 tracked 文件保持原内容：

| 场景 | 暂存文件数 | 工作区文件数 | 增删行期望 |
| --- | ---: | ---: | --- |
| 未暂存 | 0 | 20 Modified | 工作区 +60 / -0 |
| 已暂存 | 20 Modified | 0 | 暂存 +60 / -0 |
| 未跟踪 | 0 | 20 New | 两栏统计均 +0 / -0 |

跟踪文件从原本 LF-only、UTF-8、100 KB 以下的文本中选择，涉及源码、脚本和文档，每个追加 3 行。未跟踪为新目录下 20 个各 3 行的小文本。未跟踪文件不纳入 GitUI 的 tracked 增删行统计，这不是遗漏。

最初构造诊断发现 CRLF/混合换行文本在现有 libgit2 行数统计中会计入大量换行差异，和 Git CLI 的过滤口径不同，精确断言立即终止了运行。这些诊断没有有效样本，全部排除。最终选择原本 LF-only 的文件，**没有为了测速转换源文件的换行**。本轮不是 CRLF 正确性修复或二进制 UE 资产修改基准。

## 方法

两版 Release 测试程序使用同一份扩展后的 probe（源 SHA 一致），生产文件分别与上述 commit 匹配，Cargo.lock 相同。编译 provenance 记录了各自 asyncgit 重新构建且 `fresh=false`，避免共享 target 缓存误用。详情见 [构建来源](build-provenance.json)。

每场景运行三组：baseline→candidate、candidate→baseline、baseline→candidate。每个进程 23 次完整刷新，前 2 次预热丢弃，**每场景每版本保留 63 次**；合计 378 个有效刷新样本。测量期间没有本任务的编译/测试负载。

实际 `Status::show/update`、后台扫描、选中文件 diff/preview、通知处理及组件 draw 均参与；Git 和 App 通知都被处理。完成条件是已收到状态结果、全部 pending 归零且两条通知队列均空。每轮校验状态条目数量及两栏增删行数，避免“更快但漏算”。绘制目标为 160×50 Ratatui TestBackend，不含默认 5 秒轮询等待、真实终端宿主显示和冷启动。表中是热缓存中位数；p95 为各 63 个单次样本的 nearest-rank 第 95 百分位，未删除慢样本。

硬件：Windows 11，Ryzen 7 9700X 8 核 16 线程，ZHITAI TiPlus7100 2TB NVMe；Rust 1.99.1，Release/LTO。工作区仍经 libgit2 扫描，gix 没有参与 Status 状态扫描。

## 中位耗时

| 场景 | 文件列表更新：优化前 → 后 | 耗时减少 | 全部后台任务：优化前 → 后 | 耗时减少 |
| --- | ---: | ---: | ---: | ---: |
| 未暂存修改 | 578.55 → 510.38 ms | 11.78% | 1,200.34 → 1,056.35 ms | **12.00%** |
| 已暂存修改 | 572.17 → 511.25 ms | 10.65% | 1,178.46 → 575.26 ms | **51.19%** |
| 未跟踪文件 | 576.13 → 512.82 ms | 10.99% | 1,185.22 → 512.82 ms | **56.73%** |

原因与代码路径相符：

- **未暂存**仍要计算工作区 diff 的增删行，主要省下空暂存区统计，并获得 Repository 复用收益。因此不会像干净仓库那样减半。
- **已暂存**只需相对较轻的 tree→index 行数统计，空工作区不再做另一遍全仓遍历。
- **未跟踪**不属于 tracked 行数统计，两项统计都能跳过，所以后台结束时间基本等于状态列表更新。选中文件的预览通知仍正常处理。

## 尾延迟与波动

| 场景 | 列表更新 p95：优化前 → 后 | 全部后台任务 p95：优化前 → 后 |
| --- | ---: | ---: |
| 未暂存 | 699.31 → 876.16 ms | **1,435.29 → 1,744.72 ms** |
| 已暂存 | 630.06 → 567.40 ms | 1,317.96 → 643.09 ms |
| 未跟踪 | 690.76 → 660.22 ms | 1,322.79 → 660.23 ms |

未暂存的候选慢样本集中在第二组：三组全部任务 p95 分别为 baseline 1,778 / 1,328 / 1,349 ms，candidate 1,118 / 1,850 / 1,178 ms；候选三组中位数均更快。数据表现为某一时段的集中波动，但没有系统级采样证据可以确认原因。保留并报告总体 p95 回退，**不据此承诺未暂存场景的尾延迟稳定改善，也不把慢样本删除后重算**。

## 原仓库保护核对

正式测量前后完全相同：原 HEAD、索引 SHA256 和 mtime、config SHA256、完整 porcelain 状态、全部 tracked 文件的 `(路径, 大小, mtime)` 清单 SHA256，以及所选 20 个原文本文件的内容 SHA256。原仓库一直保持干净。所有场景写入只发生在独立副本；副本自身的暂存只写自己的对象库和索引。

归档中的 20 个源文件用 `file-00` 等标识保留内容哈希，未公开项目源码内容、选择文件路径清单或完整 Git 配置。38 GB 临时副本保留在 `target/status-dirty/replica/fixture`：测量完成后的递归清理被工具自动审批策略拒绝（仅返回 blocked by policy），未执行删除，未尝试绕过。可执行文件和本地诊断同样留在 ignored `target/` 中。

## 复现与文件

将同一 `src/tabs/status_perf.rs` 放到优化前后两个生产版本，分别构建并保存 Release gitui 测试 executable，然后：

```powershell
python scripts/perf_status_dirty.py prepare D:/work/ue_prj/GT_DemoB target/status-dirty-new
python scripts/perf_status_dirty.py run D:/work/ue_prj/GT_DemoB target/status-dirty-new <baseline-dirty.exe> <candidate-dirty.exe>
```

准备目录必须不存在，且位于原仓库外；需要约 38 GB 空间。目标是干净、普通 `.git` 目录的仓库，不是通用 linked worktree 测试器。输出完整 `manifest.json` 和副本原始材料留在本地 target；对外只归档数值与脱敏保护证据。

- [汇总](summary.json)、[原始样本（含预热）](samples.csv)
- [环境和原仓库保护核对](environment.json)、[构建来源](build-provenance.json)
- `unstaged/staged/untracked-0/1/2-baseline/candidate.txt`：所有进程原始输出。

此次没有新增生产优化，也没有替换当前 Scoop gitui。

验证：Status 常规测试 15 项通过；扩展 probe 每版各 3 场景 × 3 进程 × 23 轮全部通过精确状态/行数断言；Python 脚本语法校验通过。
