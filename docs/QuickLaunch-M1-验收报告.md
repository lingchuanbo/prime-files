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


---

# 复查（第 2 轮，2026-11-22 21:00）

> 审查对象：`feature/quick-launch-m1` 分支，相对 `main`（eac7738）的 10 个提交。工作区没有未提交的改动。
> 方式：静态审查。从 git 对象库直接读每个提交的文件树，逐个对比；`cargo` 命令仍由执行方运行。
> 结论：**有条件通过**。报告点名的 13 项都已修复；还需要处理 3 处小问题（R1–R3）、补 2 项证据（E1–E2），完成后即可合入。

## A. 上轮问题复核

| # | 结果 | 依据 |
|---|---|---|
| B1 | ✅ | 已拆成 5 个提交：格式化、Ctrl+D 菜单位置、侧栏开关、dist 构建配置、M1，另有 5 个修复提交。逐文件核对过格式化提交：只有换行、补逗号和分号、`else { continue; }`，没有逻辑改动 |
| B2 | ✅ 代码已改，⚠ 待手测 | 打开文件夹前先调用 `tray::request_show()`。但这个请求要等 root 的 300 ms 循环才会执行，而标签页会先在隐藏的窗口里打开；快速启动窗口一隐藏，进程可能失去前台权限。需要按 E2 手测：主窗口要能出现**在最前面**。如果只在任务栏闪烁，改为在 `apply_reply` 里直接调用托盘的 `show_main` 显示主窗口，不再经过标志位 |
| B3 | ✅ | 分成 `quick-launch` 和 `quick-launch-icons` 两条线程；图标请求排队超过 2 s 就丢弃；新增测试 `a_blocked_icon_thread_does_not_delay_a_launch`：图标线程被条件变量卡住时，启动请求在 100 ms 内得到回复 |
| S1 | ✅ | 节流保存改为 `save_in_background`；退出时 `flush_pending_save` 仍然同步写盘 |
| S2 | ✅ | 进程扫描加了 100 ms 上限 |
| S3 | ✅ | 用 `GetCursorPos` + `MonitorFromPoint` 取得显示器在 `EnumDisplayMonitors` 中的序号。已核对 gpui 的 Windows `displays()`，`DisplayId` 正是这个序号，两边映射一致 |
| S4 | ✅ | panic 后的回复带上请求的 generation，有测试 |
| S5 | ✅ | 两条线程各等 250 ms 后再 join；托盘的“退出”路径也调用 `shutdown` |
| S6 | ✅ | 改成按 `revision` 缓存 `Frame`，每帧不再克隆整份配置（问题见 R1） |
| S7 | ✅ | 只有 `Started` 或 `Activated` 才计入使用次数（文件夹条目从不计数，记入 M2 待办） |
| S8 | ✅ | 注释已改正 |
| S9 | ✅ | `??` 乱码已清除 |
| S10 | ✅ | 已改为 `.log_err()` |

## B. 对执行方“未决问题”的答复

| # | 答复 |
|---|---|
| 1 失败提示改为底部一行 | **接受**。规格 §8.5 已同步改成“窗口底部一行提示” |
| 2 网络盘卡死只有单元测试 | 补一次简单的真机测试即可：把某个条目的 `icon` 设为 `\\10.255.255.1\x\a.exe`（不可达地址，SMB 会卡 20 秒以上），打开窗口后立刻点击另一个条目，应该马上启动。归入 E2 |
| 3 激活和 UAC 没有测 | **必须测**，见 E2。Notepad 换成 `C:\Windows\System32\charmap.exe`（经典 Win32 程序，测激活）和 `C:\Windows\regedit.exe`（清单要求管理员权限，会弹 UAC，测“否”） |
| 4 混合 DPI 多显示器 | 如果只有一块屏，标注“无条件，未测”，记入 M2 待办，不阻塞本次合入 |
| 5 退出预算两条线程平分 | 接受 |
| 6 BOM 导致只读 | **要修**，见 R2。Windows PowerShell 5.1 和旧版记事本写 UTF-8 时都会带 BOM，用户手改一次文件就会进入只读状态。规格 §4.2 已补上这一行 |
| 7 clippy 有 28 个 error | **需要证据**，见 E1。其中 `nohrs-core/config/paths.rs` 和 `i18n` 在 M1 提交里改过，不能直接认定无关 |
| 8 重复的规格文件 | 由 BoBO 处理 |
| 9 测试环境陷阱 | 记录得很好。请把这三条补进 `docs/testing.md`，作为本次唯一允许的文档顺手改动 |

## C. 本轮新发现，必须修

- **R1** `restore_last_group()` 修改了当前分组，但没有调用 `touch()`。如果 KV 恢复在窗口第一次绘制之后才完成，`Frame` 不会重建：分组栏和格子显示的还是旧分组，键盘选中的却已经是新分组的槽位。**修复：**在 `restore_last_group` 里，分组确实改变时执行 `slot = None` 和 `touch()`，并补一个测试。
- **R2** `read_quick_launch` 解析前去掉开头的 U+FEFF，补一个测试：带 BOM 的文件能正常解析。
- **R3** `quick_launch.rs` 第 771–779 行的文档注释错位：`touch()` 的注释开头是 `keeps_running` 的半句，`keeps_running` 自己的注释被截断了。改正即可。

## D. 需补交的证据

- **E1** 分别在 `main`（eac7738）和本分支上运行 `cargo clippy --release --workspace --all-targets -- -D warnings 2>&1 | findstr /C:"error"`，贴出两份完整输出。两边的 error 列表完全相同，才算“既有问题”；本分支多出来的 error 必须修。
- **E2** 真机手测，逐项写通过、失败或未测：
  1. 主窗口隐藏在托盘时，从快速启动打开文件夹：主窗口**出现在最前面**，并停在新标签上（`close_to_tray` 为 true、false 各测一次）
  2. 先打开 charmap，再从快速启动点击 charmap 条目：激活已有窗口，**不会**启动第二个
  3. 打开 regedit 条目，在 UAC 中选“否”：没有任何错误提示
  4. 不可达 UNC 图标卡住时，点击其他条目能立即启动
  5. 上一轮 §4 中尚未报告的项目：12 种设置组合下关闭主窗口、热键连按 50 次、两个热键同时可用、`keep_running = false` 时 1 秒内退出

R1–R3 修完、E1 和 E2 全部通过后，就可以合入 `main`。
