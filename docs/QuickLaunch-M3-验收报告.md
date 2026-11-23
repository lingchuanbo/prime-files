# 快速启动 M3 验收报告

> 审查日期：2026-11-23
> 审查对象：`feature/quick-launch-m3` 分支，基于 `main`（c93165c），共 9 个提交（80bc153 … 7462337）。提交里的文件都是 LF；工作区签出成了 CRLF，换算后内容与 HEAD 完全一致。
> 依据：规格 v2.6 §17.1；执行方报告（fmt、clippy、test 输出和未决问题）。
> 方式：静态审查，直接读取 git 对象；cargo 结果由执行方提供。
> 结论：**有条件通过**。A–E4 的功能都已实现，结构也对。需要处理 **3 项必修（B1–B3）**：其中 B1 可能清空使用次数；B2 发现自 M1 起 clippy 从来没检查过快速启动代码。另有 4 项应改（S1–S4）。

---

## 1. 已确认的部分

| 项 | 结果 | 依据 |
|---|---|---|
| 第 0 步 | ✅ | 规格 v2.6 已提交到 M3 分支 |
| A | ✅ | `item_for_path` 移到 `nohrs-core`；`write_atomically` 移到 `nohrs-services::fs::atomic`，`nohrs-pages` 改为调用新位置 |
| B 设置页 | ✅（见 S1、S3） | 浏览按钮在 `spawn_in` 里回填输入框；打开 `quicklaunch.toml` 在后台交给 `LaunchRegistry`；删除分组需要二次确认 |
| C 命令行 | ✅（见 S2） | 不打开 redb；写入使用 `write_atomically`；配置解析失败时退出码为 2；对文件夹的 `run` 交给同目录的 `LumaFiles.exe`；COM 使用 RAII 守卫 |
| D `@` 搜索 | ✅ | 只有以 `@` 开头时才走 `rank_quick_launch`；**不带 `@` 的分支一行没改**（逐行对比过 view.rs 和 ranking.rs）；文件结果的 `quick_launch` 固定为 `None`；启动和打开所在位置都通过钩子，并在之后关闭 Launcher |
| E1 | ✅ | `exe,索引` 的索引号计入缓存键 |
| E3 / E4 | ✅ | `docs/launcher.md` §14；schema 有一致性测试 |
| 测试 | ✅（除既有问题外） | 共 915 个通过；`nohrs-cli` 的 4 个日志测试失败，在 `main` 上同样失败，相关文件自 `main` 以来没有改动（已核实），不是 M3 引入的 |

---

## 2. 必须修复

### B1. E2 的清理会在“文件暂时不存在”时删掉全部使用次数
`apply_loaded()` 在所有情况下最后都会调用 `prune_usage()`，包括：
- **文件不存在**（`parsed = None`）：配置会被换成只有一个空分组的默认配置，于是“有效条目”集合为空，**所有 `usage_*` 都会被删掉**。
  这个情况并不少见：很多编辑器保存文件的方式是先删除、再写入。监视器可能正好在两步之间触发重载，读到“文件不存在”。第一次启动、或者用户暂时把文件挪走，也会这样。
- **解析失败（只读状态）**：配置保留的是上一次的内容，这时用它去判断哪些记录“没有对应条目”，没有依据。

**修复**：只有在本次读取是 `Some(Ok(parsed))` 时才调用 `prune_usage`；文件不存在和解析失败都跳过。
**测试**：文件不存在时触发重载，使用次数记录保持不变；解析失败时也保持不变；文件正常时，没有对应条目的记录被删除。

### B2. 从 M1 起，clippy 从来没有检查过快速启动代码；这一轮暴露出一个忽略 `Result` 的问题
- 执行方每轮贴出的 clippy 输出里都有 `could not compile nohrs-core (lib) due to 5 previous errors`。`nohrs-core` 在 clippy 下编译失败，所以**依赖它的 crate（`nohrs-services`、`nohrs-launcher`、`nohrs-pages`、`nohrs-cli`、`nohrs`）一个都没被检查过**。“与 main 对比完全相同”只能说明 `nohrs-core` 和 `nohrs-indexer` 没有变化，不能说明快速启动代码是干净的。这一点我前两轮审查也漏看了。
- 实际漏掉的问题：`quick_launch.rs` 中 `retype_after_edit` 的 `cx.update(|cx| { … });` 返回值被直接丢弃（rustc 会给出 unused `Result` 警告），等于另一种形式的 `let _ =`。这段代码是 M2 S4 加进来的，现在已经在 main 上。
- **修复**：
  1. 这一处改为 `.log_err()`。
  2. 改用 `--no-deps` 单独检查下游 crate，在 main 和本分支上分别运行，贴出两份完整输出：
     ```
     cargo clippy --release -p nohrs-services -p nohrs-launcher -p nohrs-pages -p nohrs-cli -p nohrs --all-targets --no-deps -- -D warnings 2>&1 | findstr /C:"error" /C:"warning:"
     ```
  3. 本分支相比 main 多出的条目，以及**落在快速启动相关文件里的全部条目**（`quick_launch*.rs`、`settings/quick_launch.rs`、`launch*.rs`、`nohrs-cli/src/quick_launch.rs`、`hotkey.rs`、`ranking.rs`、`view.rs`、`app.rs`，即使在 main 上已经存在），都必须修掉。其他文件里本来就有的问题只记录，不修。
  4. 如果 `--no-deps` 仍然因为 `nohrs-core` 的问题无法完成，在“未决问题”里写明原因，并改为在 `nohrs-pages` 等 crate 上用 `cargo build --release` 检查 rustc 警告，同样逐条对比。

### B3. 4 个日志测试失败，需要登记为独立问题（不阻塞本次合入）
从执行方的描述（“Windows 上 `log_dir()` 用的是可执行文件目录”）看，原因与执行方在 M3 CLI 测试里遇到的是**同一个便携模式陷阱**：`target\release\userdata\config.toml` 打开了 `portable_mode`，导致 `XDG_*` 被忽略。这是测试环境问题，不是 M3 引入的。
**处理**：本轮不修代码。在 `docs/testing.md` §7 补一条：`nohrs-cli` 的这 4 个测试在开发机上会因为便携模式而失败，运行前要先把 `target\release\userdata` 改名，或者在 CI 的干净环境里运行。另外在 PR 描述中把它列为已知问题，之后单独处理。

---

## 3. 应该修复（与 B 项一起做）

| # | 问题 | 修复 |
|---|---|---|
| S1 | 热键格式转换有两份相同的实现（launcher 和 settings 各一份）。原因是**规格 §17.1 写错了**：设置页在 `nohrs-pages` 里，而 `nohrs-pages` 不能依赖 `nohrs-launcher`。这是规格方的问题 | 把 `gpui_keys_to_hotkey` 移到 `nohrs-core`（例如 `nohrs_core::shortcuts`），删掉两份副本，测试一并移过去。规格同步更正（v2.7） |
| S2 | CLI 为了 COM 新加了 `windows` 依赖和一段 `unsafe`，让 `unsafe` 出现在规格 §8.4 没有列出的位置 | 在 `nohrs-services/src/launch/windows.rs` 里公开一个 `ComApartment::enter()` RAII 守卫（`launch.rs` 在非 Windows 平台提供空实现）。CLI 改用它，删掉 `nohrs-cli` 的 `windows` 依赖和其中的 `unsafe` |
| S3 | “和搜索启动器热键冲突”的判断依赖错误文本里的 `"Launcher's own shortcut"` 字样，错误文案一改，判断就会悄悄失效 | `HotkeyHooks.register` 的错误类型改为枚举 `HotkeyRefusal { LauncherConflict, Other(String) }`，由 `app.rs` 构造，设置页按枚举判断 |
| S4 | 执行方未决问题 4：CLI 测试需要把二进制复制到临时目录，否则会读写开发者自己的配置 | 和 B3 一起写进 `docs/testing.md` §7 |

---

## 4. 对执行方未决问题的答复

| # | 答复 |
|---|---|
| 1 热键转换重复 | 见 S1，是规格的问题 |
| 2 冲突靠错误文本判断 | 见 S3 |
| 3 CLI 的 windows 依赖 | 见 S2 |
| 4 CLI 测试要复制二进制 | 接受，见 S4 |
| 5 空分组删除也二次确认 | **接受**（更保守） |
| 6 不支持拖动排序 | 接受（D3） |
| 7 `run` 依赖同目录的 `LumaFiles.exe` | 接受（D2） |
| 8 `normalize_schema` 改为 `pub(crate)` | 接受，属于 E4 所必需的最小改动 |
| 9 `retype_after_edit` 的警告 | **不接受“既有、不动”**。这是 M2 引入的快速启动代码，必须修，见 B2 |
| 10 手测 | 由 BoBO 执行 §17.1 G 的 6 项 |
| （补充）`activate_by_id` 会把快速启动窗口切到该条目所在的分组 | 接受。相当于在窗口里点了这个条目，而且不会写入 `last_group` |

---

## 5. BoBO 手测（§17.1 G）

可以直接用 08:51 编译的版本测试，B1–B3 和 S1–S4 都不会影响下面这些项目的表现：

1. 设置 → 快速启动 → 录制一个新热键：新热键立即生效，旧热键失效；录制成 `Ctrl+Shift+Space` 时提示冲突。
2. 在设置页修改某个条目的名称或图标：快速启动窗口立即更新。
3. 设置页的修改在 1 秒内写进 `target\release\userdata\quicklaunch.toml`。
4. 在 Launcher（`Ctrl+Shift+Space`）里输入 `@ps` 能找到 Photoshop，回车启动；只输入 `@` 能列出全部条目。
5. 在终端运行 `target\release\noh.exe ql add "C:\Windows\System32\charmap.exe"`：正在运行的快速启动窗口里马上出现这个条目（`noh.exe` 和 `LumaFiles.exe` 在同一个目录，读写的是同一个便携配置）。
6. 在设置页的搜索框里输入“**快捷键**”或“全局”，能找到快速启动的“全局快捷键”这一行（中文翻译统一用“快捷键”，不用“热键”）。

---

## 6. 合入条件
B1、B2 修好，S1–S4 一起处理，B3 写进文档；执行方贴出原始输出（fmt、**`--no-deps` 的 clippy 对比**、test）；BoBO 完成第 5 节的 6 项手测。以上都满足后即可合入 `main`。


## 7. 手测结果（BoBO，2026-11-23 09:10，08:51 编译的版本）

| # | 项目 | 结果 |
|---|---|---|
| 1 | 设置页录制热键立即生效，与 Launcher 冲突时有提示 | ✅ |
| 2 | 设置页改名称或图标，窗口立即更新 | ✅ |
| 3 | 设置页的修改 1 秒内写入文件 | ✅ |
| 4 | Launcher 中用 `@ps` 找到并启动；单独输入 `@` 列出全部 | ✅ |
| 5 | `noh ql add` 添加的条目立即出现在窗口里 | ✅ |
| 6 | 在设置页搜索“热键” | ⚠ 搜不到。**不是缺陷**：中文翻译统一用“快捷键”（这一行叫“全局快捷键”，翻译里没有“热键”这个词），是第 5 节第 6 项的写法有误，已更正为搜“快捷键”。BoBO 决定不为“热键”加搜索别名 |


---

# 复查（第 2 轮，2026-11-23 10:00）

> 审查对象：`feature/quick-launch-m3` 在 E4 之后新增的 7 个提交（c0fe5b9 … c034b1a）。
> 历史改写：执行方用 cherry-pick 重排了 C 之后的提交，C、D、E1–E4 的哈希都变了。我逐个文件比较了改写前的 E4（7462337）和改写后的 E4（5575eb2），**唯一的差别是 `crates/nohrs-cli/tests/quick_launch.rs`**（测试用二进制副本的目录命名方式，见执行方未决问题 3），可以接受。
> 结论：**通过**，合入前只需改 1 处小问题（R1）。

## A. 逐项确认

| 项 | 结果 | 依据 |
|---|---|---|
| B1 | ✅ | `apply_loaded` 只在 `Some(Ok(parsed))` 时设置 `parsed_ok`，之后才调用 `prune_usage`；三个测试都在（`a_missing_file_…`、`a_broken_file_…`、`a_load_deletes_…`） |
| B2 | ✅ | `retype_after_edit` 改为 `.log_err()`。逐包运行 `--no-deps` clippy，快速启动相关文件里查出 9 条从 M2 起就漏检的问题，已全部修复，本分支相比 main 没有新增。`nohrs` 的 build script 本身无法通过 clippy，`app.rs` 改用 `cargo build` 检查，没有警告 |
| B3+S4 | ✅ | `docs/testing.md` §7 已补充 |
| S1 | ✅ | `nohrs_core::shortcuts::gpui_keys_to_hotkey` 是唯一的实现，launcher 和 settings 中的副本都已删除。**顺带修掉了一个隐藏 bug**：settings 原来那份把 Win（`platform/cmd/super`）当成 Ctrl 处理，现在正确输出 `super`。备注：settings 副本的删除放在了 S3 提交里，不是 S1，不影响结果 |
| S2 | ✅ | `nohrs_services::launch::ComApartment` 是 RAII 守卫，非 Windows 平台为空实现；CLI 不再依赖 `windows`，也没有 `unsafe` |
| S3 | ✅ | 新增枚举 `HotkeyRefusal { LauncherConflict, Other }`，由 `app.rs` 构造；设置页改为按枚举判断 |

## B. 合入前修改

- **R1** `crates/nohrs-services/src/launch.rs` 第 24–25 行，`#[cfg(windows)]` 重复写了两次（S2 引入）。删掉一行即可。

## C. 对未决问题的答复

| # | 答复 |
|---|---|
| 1 `--no-deps` 卡在 build script | 接受逐包运行的做法。清理 `build.rs`、`nohrs-indexer`、`nohrs-core` 中已有的 clippy 问题，**另开一项**处理（这是让 clippy 能作为合入检查的前提） |
| 2 既有问题记录、不修 | 接受 |
| 3 测试用二进制副本的命名 | 接受（按大小 + 修改时间命名并复用，避开杀毒扫描） |
| 4 COM 守卫有两份 | 接受，记入后续待办：`nohrs-pages` 的 worker 线程改用 services 的守卫 |
| 5 `unregister` 仍返回 String | 接受 |
| 6 launcher 保留一个调用 `parse` 的测试 | 接受，它测的就是 core 里那份共享函数 |
| 7 B1 的代价 | 接受（陈旧记录会留到下一次成功读取时再清理，不会误删） |
| 8 文档提交包含手测结果 | 接受 |
| 9 9 条漏检问题已修 | 确认 |
| 10 手测 | 已在第 7 节记录全部通过。本轮改动不影响手测项目，不需要重测 |

## D. 合入条件
修好 R1，fmt 和 `cargo build --release -p nohrs` 都通过，就可以合入 `main`。
