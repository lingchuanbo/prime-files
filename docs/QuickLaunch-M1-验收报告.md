# 快速启动 M1 验收报告

> 审查日期：2026-11-22
> 审查范围：15:06 提交（“更新优化”）之后工作区内的全部改动
> 审查依据：`docs/QuickDock-技术实现方案与边界约定.md` v2.1
> 审查方式：静态审查。逐个对比 git 索引中的原版本和工作区内容，代码逐段阅读。**没有运行** fmt、clippy、test，也没有启动程序。
> 结论：**暂不通过**。功能主体质量不错，但有 3 个问题必须先处理（见 §2），另有 3 项需要提供材料（见 §4）。

---

## 1. 符合规格的部分（这些不要再改）

| 项目 | 结果 |
|---|---|
| 新增文件 | 5 个新文件的位置都符合 §3.1；没有 `mod.rs`；新代码里没有用 `dock` 命名 |
| 错误处理 | 生产代码中 `unwrap()`、`let _ =` 都是 0；`expect` 只出现在测试里 |
| unsafe | 只出现在 `launch/windows.rs` 和 `quick_launch.rs` 的窗口函数里，都是逐项 `#[allow(unsafe_code)]`，每处都有 `// SAFETY:` 说明 |
| 热键重构（§10） | 整个进程只有一个 manager、一个 40 ms 轮询，按 id 分发；原有测试全部保留，新增 3 个测试；拒绝和 Launcher 热键冲突的组合；改热键失败时保留旧热键 |
| 配置模型（§4） | §4.2 表格中每种情况都有测试（共 36 个）；更新的 schema 会被拒绝，不会被自动修补；`keep_running` 默认为 true |
| 读写（§5） | 原子写入流程为 tmp → `sync_all` → `.bak` → rename；解析失败时进入只读状态，禁止保存；用内容哈希跳过没有变化的重载；复用 root 现有的 400 ms 循环，**没有新增定时器** |
| KV（§6） | 键都通过 `KvKey::new("quick_launch", …)` 构造，格式符合规则 |
| 启动（§8） | 统一使用 `ShellExecuteExW`；HRESULT 中的 1223 被正确识别为“用户取消”；URL 只放行 http、https、mailto |
| 复用 | `thumbnails::platform`、`tray::platform` 只是把可见性改成 `pub(crate)`，没有复制代码 |
| 常驻（§15.0） | `close_action` 写成纯函数，并按规格表格逐行测试；托盘的“退出”和标题栏“⋯”菜单都是通过已有的标志位传递 |
| i18n（§14） | 新增的 7 条文案，9 种语言都有，没有乱码 |
| 编译 | `target/release` 下 `LumaFiles.exe` 和测试二进制都是 18:40 编译的，晚于最后一次源码修改（18:39） |

---

## 2. 必须处理（不处理不予通过）

### B1. 改动超出 M1 范围
除了快速启动本身，工作区里还有以下改动，都不在规格 §3.2 的允许清单内：

| 改动 | 涉及文件 |
|---|---|
| **Ctrl+D 目录菜单位置**新设置（`directory_menu_position`，可选“鼠标处”或“列表左上角”） | `nohrs-core/config/settings.rs`、`nohrs-pages/settings.rs`、`explorer/state.rs`、`explorer/context_menu.rs`、`explorer/marquee.rs`、`docs/config.schema.json`、快照文件 |
| **分栏侧栏布局**（`layout_applied`），以及约 500 行新测试 | `explorer/page.rs`、`explorer/tests.rs` |
| 安装包构建配置 `[profile.dist]` 改为 `codegen-units = 1`、`lto = "fat"`、`debug = "line-tables-only"` | `Cargo.toml` |
| 全仓 rustfmt 重排：约 50 个文件只有格式变化（换行、import 排序、去掉多余花括号） | 遍布各 crate |

**处理方式：**如果这些是 BoBO 另外安排的工作，请分成单独的提交。如果是执行方顺手改的，请**撤回到独立分支**。rustfmt 重排也要**单独提交一次**，不要和功能改动混在一起，否则 M1 的 diff 没法审。

### B2. 主窗口隐藏在托盘时，打开文件夹条目会看不到结果（极可能是缺陷，需手测确认）
调用链：`apply_reply` → `explorer_link::request(path)` → `RootView::show_path_from_elsewhere` → `open_path` + `activate_window` + `foreground::bring_to_front`。
这条链路**从头到尾没有调用 `ShowWindow(SW_SHOW)`**。已确认 `bring_to_front` 只调用了 `SetForegroundWindow` 和 `BringWindowToTop`。Q5 确认常驻后，“主窗口隐藏在托盘”会成为**常态**。这时点击文件夹条目，标签页会在一个看不见的窗口里打开，用户看到的就是“什么都没发生”；托盘的 `HIDDEN` 状态也不会更新。

**修复：**在 `quick_launch.rs` 的 `apply_reply` 中，处理 `OpenFolder` 时，在调用 `explorer_link::request` **之前**先调用 `crate::tray::request_show()`。这样不需要修改 `explorer_link.rs`。

**验收：**分别在 `close_to_tray` 为 true 和 false 两种设置下关闭主窗口，再从快速启动打开一个文件夹，主窗口应该出现，并停在新标签上。

### B3. 图标提取会堵住启动请求（规格 §8.4 的设计缺陷，由规格方负责）
§8.4 要求“启动和图标提取放在**同一条**专用线程上”，执行方照做了。问题是：
- 第一次打开窗口时，每个条目都会入队一个图标请求，比如 60 个图标，每个 20–50 ms。这时用户**立刻点击**，启动请求要排在所有图标请求后面，会延迟 1–3 秒才启动。
- 如果某个图标路径在断开的网络盘上，Shell 调用会卡住，**之后所有启动请求都会卡住**。2 秒超时只是让界面不再等这个图标，并不能让线程恢复。

**修复（规格同步修订为 §8.4 v2.2）：**拆成两条专用线程：
- `quick-launch`：只处理 `Launch`，空闲时等待请求。
- `quick-launch-icons`：只处理 `Icon`。图标请求如果在**队列里**已经等待超过 2 秒就直接丢弃，不再执行；线程卡住时，界面继续显示占位图标，启动不受影响。

两条线程各自初始化 COM，退出时各自释放。

**验收：**新增一个测试，用一个阻塞的假图标提取器卡住图标线程，验证启动请求仍然能在 100 ms 内得到回复。

---

## 3. 应该修复（可以在 M2 开始时一起改，但不能留到 M2 结束）

| # | 问题 | 位置 | 修复方式 |
|---|---|---|---|
| S1 | 节流保存 `poll_save → save_now` 在**前台线程**同步写盘，违反 §5.2 的“在后台保存”。M1 还没有编辑功能，所以暂时不会触发，但 M2 加入拖放以后会 | `quick_launch.rs:1041–1117` | 节流到期后先把内容渲染成文本，再用 `cx.background_spawn` 写入，完成后回到前台更新 `file_hash`。退出路径 `flush_pending_save` 可以保留同步写入 |
| S2 | “已在运行”的检测会对每个进程调用 `OpenProcess`，**没有** §8.3 规定的 100 ms 上限 | `launch/windows.rs:228` | 在循环里检查耗时，超过 100 ms 就返回 `None`，直接启动新实例 |
| S3 | 打开窗口时按 `cx.active_window()` 的鼠标位置决定显示器。用热键召唤时，前台是其他程序，这个值为 `None`，结果**总是出现在主显示器**，而不是鼠标所在的显示器 | `quick_launch.rs:1155` | Windows 上改用 `GetCursorPos` 加 `MonitorFromPoint` 获取鼠标所在显示器 |
| S4 | 工作线程 panic 后的回复用的是 `generation: 0`，会被当作过期回复丢掉，用户看不到任何提示 | `quick_launch.rs:307` | 改为携带当前请求的 generation |
| S5 | 退出时没有等待工作线程（§15 要求最多等 500 ms）；托盘“退出”路径也没有调用 `quick_launch::shutdown` | `shutdown()`、`root.rs` 中 `take_quit` 分支 | 补上 join，最多等 500 ms；`take_quit` 分支也调用 `shutdown` |
| S6 | 每一帧都克隆整份 `QuickLaunchConfig` 和当前分组的条目 | `view.rs:185–227` | 只取当前分组条目的引用或 `Arc`，或者只在配置变化时缓存一份 |
| S7 | 启动失败时也会给使用次数加 1 | `activate()` | 改为收到 `Started` 或 `Activated` 回复后再记录 |
| S8 | `explorer_link::request` 返回 false 时，注释写的是“交给系统处理”，但代码实际只写了一条日志 | `quick_launch.rs:1536` | M1 中主程序一定存在，只需改正注释；真正的兜底按 §15.1 在 M2 实现 |
| S9 | 注释里有乱码 `??`（原本是破折号） | `launch.rs`、`launch/windows.rs` 第 8–9 行 | 改回 `—` |
| S10 | `ShowWindow(...).ok().ok()` 会静默丢弃错误 | `launch/windows.rs:323` | 改用 `.log_err()`，或者写明为什么可以忽略 |

---

## 4. 需要补交的材料（规格 §1 第 6 条、§16）

我这边无法运行命令，以下内容请执行方贴出原始输出：

1. `cargo fmt --all -- --check`、`cargo clippy --release --workspace --all-targets -- -D warnings`、`cargo test …` 的完整结果。
2. §16.2 性能数字：从召唤到窗口出现、第一次召唤、200 个条目时切换分组、解析 500 个条目。日志里已经有 `summon elapsed_ms`，可以直接读取。
3. §16.3 手动测试：带 `#[ignore]` 的 Win32 测试；12 种设置组合下关闭主窗口的表现；热键连按 50 次；Launcher 热键和快速启动热键同时可用；`keep_running = false` 时进程在 1 秒内退出。

---

## 5. 其他
- `docs/` 下有两份规格：`QuickDock-技术实现方案与边界约定.md` 和 `快速启动技术规格 v2.1.md`，内容相同。建议删掉后者，避免执行方读到旧版本。
- 复查时只需要看第 2 节和第 3 节的改动，以及第 4 节的材料。
