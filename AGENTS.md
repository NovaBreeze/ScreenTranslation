# ScreenTranslator 开发约定

## 提交纪律（必须遵守）

- **每次新增功能、修复 bug 后必须立即 `git commit`**：一个变更一个提交，不积压、不攒批。
- 提交前必须完成验证：`cargo test --release` 全绿，且 `cargo build --release` 零警告。
- 提交信息写清「做了什么、为什么」；关联的 UI、文档、测试与代码随同一提交。
- 不提交与本次变更无关的文件；验证截图、临时诊断产物（如 `probe/`）不入库。
- 用户自己未提交的改动（如 README.md）不得混入功能提交。

## 常用命令

- 构建：`cargo build --release`（i5-9400F 全量约 4-6 分钟；构建前必须停掉运行中的实例，否则 os error 5）
- 测试：`cargo test --release`
- 无头 UI 验证：`cargo run --example render_overlay -- probe`
- 交互冒烟探针：`./target/release/examples/smoke_interact.exe <hotkey|drag|toggle|copy|close|click X Y|esc|shot PATH>`

## 核心约束

- 遮罩出现/交互/关闭不得有任何动画或黑闪；翻译结果直接渲染在原遮罩上，不用独立结果窗口。
- 渲染字号的两遍渲染 + `unify_sizes` 机制不得改动。
- 托盘常驻程序任务栏不出现图标。
- 干净切换：删死代码不留兼容层；零编译警告。
