# 2026-10-28 依赖升级性能实验（已回退）

本目录保留 gix 0.90.0 / ratatui-textarea 0.9.3 的升级实验与原始性能数据。由于输入框性能回退，已恢复 gix 0.86.0 / ratatui-textarea 0.8.0；上一轮依赖更新、官方 CI/README 更新和文件占用提示修复保留。

- [完整报告](report.md)：三组对比、绝对耗时、复测结果及测量限制。
- [主测量汇总](summary.csv) / [原始样本](samples.csv)：21 轮、1,386 条样本。
- [复测汇总](confirmation/summary.csv)：额外六组三版本交错复测。
- [测量环境与版本](environment.json) / [固定仓库信息](fixture_manifest.json)。

对比版本为 `061eef1b`（最初依赖升级前）、`1c254d91`（上一轮兼容依赖更新）与 `78a8d869`（升级实验及公共测试代码）。记录中的时间、版本和二进制哈希均保持原样；保存的旧绝对路径仅用于追溯原始测量环境。

复现代码保留在 `scripts/perf_compare.py`、`asyncgit/examples/perf_compare.rs` 和 `examples/perf_textarea.rs`。重建三个版本时应使用对应提交、相同测试代码与编译参数；按自己的工作区调整报告中的路径。编译产物、大型测试仓库及完整构建日志仍保留在本机 `target/perf-20261028`，没有加入 Git。
