# 批量重命名目录内文件实施计划

目标：扩展现有 Ctrl+M 窗口，在所选项目与目录内文件之间切换，支持递归、格式筛选、勾选、按目录编号和状态恢复。

边界：保持原有重命名、拼音规则和事务撤销；目录模式只处理普通文件，不递归链接或 Windows 重解析目录。不新增依赖，不修改公共模型。

执行顺序：

- [x] 扫描：先编写临时文件系统测试，验证递归开关、格式、重复目录、取消、无效输入、链接和读取失败。后台扫描，取消后丢弃部分结果，结果稳定排序。
- [x] 预览：勾选决定编号、冲突检查与执行范围；每目录编号单独计数。显示相对目录和完整路径提示。
- [x] 窗口：添加目录/格式历史输入、浏览按钮、递归开关、进度与取消；规则更改只更新预览，不重复扫描；扫描结束才能执行。
- [x] 持久化：扩展私有 Session（serde default 兼容旧文件），记住范围/筛选/编号选项；本次选择优先作为目录输入。
- [x] 回归：运行 cargo test -p nohrs-pages batch_rename --lib、定向 rustfmt、cargo clippy -p nohrs-pages --lib 和 git diff --check，构建独立新版 exe。

风险与验证：用真实文件验证跨目录同名不冲突、取消勾选释放冲突、执行前二次检查、递归重命名与撤销；GPUI 测试验证交互、布局、保存与旧状态兼容。扫描失败记录路径与原因。现有用户程序不自动终止。

验证记录（2026-11-15）：

- `cargo test -p nohrs-pages batch_rename --lib`：36 通过，0 失败。
- 定向 `rustfmt --check --edition 2024 --config skip_children=true`：退出码 0；稳定版提示两个 nightly 配置项不可用。
- `cargo clippy -p nohrs-pages --lib`：退出码 0；22 条既有警告，本次 batch_rename 模块无警告。
- `git diff --check`：退出码 0。
- release 构建成功：`target/release/nohrs-rename-directories.exe`。
- 审查勾选范围、冲突检查、后台任务取消与过期结果隔离、路径规范化、状态兼容和撤销路径；相关行为由定向测试验证。
- GUI 限制：独立测试进程成功创建窗口，但 Computer Use 截图连续两次报 `window capture timed out: timed out waiting on channel`，未完成实际界面目视验收。已关闭本任务测试进程，保留用户原有进程（PID 7352）。布局与输入交互通过 GPUI 自动测试。
- 用户原程序仍运行旧版；退出原程序后运行独立新版即可体验。


后续优化（2026-11-15）：

- [x] 将“每个目录独立编号”移到顶部处理范围区，说明起始 0、位数 5 时 A\00000.png 与 B\00000.png 的效果；沿用已保存的选项，取消勾选仍为跨目录连续编号。
- [x] 文件列表焦点下 Backspace 返回当前目录的上一级（已按用户澄清修正，不返回历史记录）；输入框继续删除文字，保留 macOS 原有按键行为。
- 验证：重命名回归 37 通过；Backspace 导航与输入框删除的 GPUI 测试 1 通过。定向 rustfmt、git diff --check、clippy 退出码均为 0（clippy 保留既有 22 条页面模块警告）。
- 构建：`cargo rustc -p nohrs --release --bin nohrs -- -o target/release/nohrs-rename-navigation.exe` 成功；未终止用户原有程序（PID 11428）。
- 本轮使用 rust-skills、verification-before-completion，并人工复核选项位置、预览更新链路、焦点与修饰键保护；未执行真实窗口目视验收。

Backspace 语义修正（2026-11-15）：改为调用 go_up 返回父目录。测试先使用“前一个浏览目录与父目录不同”的场景复现错误，再验证修正通过；同一测试确认输入框 Backspace 正常删除字符，另一个历史前进后退回归测试通过。本轮使用 systematic-debugging、rust-skills、verification-before-completion。


重命名范围区域精简（2026-11-15）：

- [x] 按用户确认移除“目标目录”整行（目录输入、历史下拉、浏览按钮），清理仅供该按钮使用的目录选择回调。
- [x] 调整处理范围说明和空结果提示；扫描继续沿用主界面当前所选内容推导的目录，保留递归、格式筛选、每目录编号和预览所在目录列。
- 验证计划：现有重命名回归（含 GPUI 布局与目录扫描）、定向格式检查、clippy、diff 检查、release 构建。局部视图删减不新增重复测试，不更改状态保存结构。
- 验证结果：重命名测试 37 通过、0 失败；定向 rustfmt、git diff --check、clippy 退出码 0（保留既有警告）；release 构建成功，输出 target/release/nohrs-rename-navigation.exe。人工复核目录初始化和窗口复用仍随当前选择更新，未进行真实窗口目视验收。
