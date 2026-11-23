# 快速启动 M4 验收报告

> 审查日期：2026-11-23
> 审查对象：`feature/quick-launch-m4`，基于 `main`（c8c0dd4），共 13 个提交（8586a2b … eb0d308）。工作区与 HEAD 一致，按 LF 比较。
> 依据：规格 v2.8 §17.2，以及执行方提交的验证原始输出。
> 方式：静态审查，直接读取 git 对象。A–G 在 G 提交（275f0b0）时已预审过；本轮复查 H 和预审要求补做的 4 个提交，并确认 A–G 的哈希和预审时相同、没有被改写。
> 结论：**通过**。合入前只需改 1 处（R1），然后由 BoBO 完成第 4 节的 8 项手测。

---

## 1. 预审（A–G）结论回顾

| 项 | 结果 |
|---|---|
| 信任键 | ✅ SHA-256，字段之间用 NUL 分隔，取前 32 位十六进制；工作目录按原文参与计算 |
| 确认关卡 | ✅ 所有发出启动请求的路径都经过确认检查：单个条目、Launcher 的 `@`、group 启动、确认后再次启动。“切换到已运行的程序 / 再开一个”只用于 app 类型 |
| 编码 | ✅ cmd 使用 `/d /s /c "…"` 原样传参；PowerShell 使用 `-EncodedCommand`（UTF-16LE 编码后再 base64）；python 交给 Rust 转义 |
| 动态分组 | ✅ `config_for_saving` 在保存前清空动态分组的条目；7 个编辑入口都检查 `group_editable`（`update_item` 在参数校验后检查）；`prune_usage` 跳过动态分组，也不碰 `trusted_*` |
| unsafe / 依赖 | ✅ M4 新增的 `unsafe` 都在 `launch/windows.rs` 中，每处都有 SAFETY 注释；`windows` 的 feature 只加在 `nohrs-services` |

## 2. 本轮复查

| 提交 | 结果 | 依据 |
|---|---|---|
| H `35fa4c7` | ✅ | 只改了 `docs/config.md` 和 `docs/launcher.md` |
| 1 `7504c07`（单个 command 需要 `--yes`） | ✅ | 检查放在 Group 分支之后、发出启动之前；不加 `--yes` 时退出码为 1 的测试不是 `#[ignore]` |
| 2 `45263a2`（应用列表在独立线程上枚举） | ✅ | 用一次性的 `std::thread`（名为 `quick-launch-apps`），通过 `async_channel::bounded(1)` 把结果交回前台，前台只做 `recv().await`；线程创建失败时会显示错误 |
| 3 `5f39e27` | ✅ | 删除了多余的 allow 和误放的文档注释 |
| 4 `eb0d308` | ⚠ 见 R1 | 确认框改用了终端字体 `Cascadia Mono` / `Consolas` |
| 验证 | ✅ | fmt 没有输出；逐包 `--no-deps` clippy 与 main 一致（快速启动相关文件里唯一的一条是 main 上本来就有的 `shortcuts.rs:644`，不在本次改动范围内）；`cargo build` 没有 rustc 警告；测试 982 个通过、0 个失败；4 个日志测试是已知的既有问题 |

## 3. 合入前修改

- **R1**：`quick_launch/view.rs` 第 2041 行，`group_command_overlay`（一次启动多个条目时那个合并确认框）的脚本区仍然是 `.font_family("Mono")`。它和单个命令的确认框作用相同，都是**在用户确认之前把完整脚本给他看**，应该用同一种字体。改成 `.font(crate::terminal::code_font())`。（执行方未决问题 3 说它是“编辑浮层”里的框，实际是合并确认框。）

## 4. BoBO 手测（§17.2 验收部分）

先在 `quicklaunch.toml` 里或者通过设置页准备好下面要用到的条目。用 **15:28 编译的版本**测试即可，R1 只改字体，不影响下面的结果。

1. 设置页 → 新建条目 → 类型选 uwp → “选择应用…” → 选中“计算器”，点击后能启动计算器。
2. 新建一个 command 条目：shell 选 `cmd`，脚本写 `echo hello & pause`。
   - **在设置页保存后，第一次点击直接运行**（D5：设置页保存的脚本自动信任），弹出控制台；
   - 用记事本把 toml 里这个脚本改成 `echo hi & pause`，保存，再点击，**应该弹出确认框**，框里显示完整脚本，确认后运行；
   - 第三次点击，直接运行。
3. 把这个 command 改为 `hidden = true`（在记事本里改），点击后确认框里有红色的“在后台运行，没有可见窗口”提示。
4. 新建一个 group，在设置页勾选 3 个条目，点击后 3 个依次启动；开启“启动后隐藏”时，窗口只在最后隐藏一次。
5. 设置页把“程序已在运行时”设为“每次询问”。先手动打开一个程序，再点击它的条目，出现三个按钮：
   - “切换到它”：切到已经打开的窗口；
   - “再开一个”：多出一个实例；
   - “取消”：什么都不发生。
6. 设置页 → 新建分组 → 选“文件夹”，指向一个工具目录，里面的程序都显示出来；在这个分组里**不能拖入、不能编辑、右键菜单里编辑类选项是灰的**；保存后看 `quicklaunch.toml`，这个分组下**没有**条目，只有 `source = "folder:…"`。
7. 新建一个“最近使用”分组，刚启动过的条目按顺序出现；在这个分组里启动某个条目后，回到它原来所在的分组，看使用次数有没有增加（可以在“最常使用”分组里观察排序变化）。
8. 在 Launcher（Ctrl+Shift+Space）里只输入 `@`：文件夹分组里的程序会出现，“最近使用 / 最常使用”里的条目**不会重复出现**。

## 5. 合入后清理

M4 合入 main 后，删除临时分支：`m3-backup`、`m3-tip-before-c-fix`、`m3-tip-second-fix`、`m4-tip-before-c-fix`、`m4-tip-before-d6`、`m4-d-amended`、`m4-e-amended`。

## 6. 对未决问题的答复

| # | 答复 |
|---|---|
| 1 `shortcuts.rs:644` | 接受，并入“清理 clippy 基线”那一项一起处理 |
| 2 两处 `--yes` 提示语 | 接受 |
| 3 另一个脚本框 | 见 R1，它是合并确认框，要改 |
| 4 枚举线程 | 确认 |
| 5 uwp 图标用占位图 | 接受，记入后续：可以用 `SHGetKnownFolderItem` 拿到 AUMID 对应的 shell item 来提取图标 |
| 6 编辑浮层不能保存 command | 接受（command 在设置页里编辑） |
| 7 工作目录按原文计算信任键 | 接受，与规格 D.1 一致 |
| 8 `on_running` 只对 `.exe` 生效 | 接受，与 §8.3 一致 |
| 9 动态分组的刷新时机 | 确认，与 G.2、G.3 一致 |
| 10 手测 | 见第 4 节 |

## 7. 手测结果（BoBO，2026-11-23 16:21，使用 15:28 编译的版本）

| # | 项目 | 结果 |
|---|---|---|
| 1 | UWP（计算器） | ✅ |
| 2 | 命令脚本与首次运行确认 | ✅ |
| 3 | 隐藏窗口运行的提示 | ✅ |
| 4 | group 一次启动多个条目 | ✅ |
| 5 | `on_running = ask` 三个按钮 | ✅ |
| 6 | 文件夹分组只读、不写回 | ✅ |
| 7 | 最近使用 / 最常使用 | ✅ |
| 8 | Launcher 里的 `@` | ⚠ **待重测**：截图显示的是**快速启动窗口**（标题“快速启动”，过滤框里是 `@`，结果 0 项），不是 Launcher。快速启动窗口的过滤只匹配名称和标签，输入 `@` 得到 0 项是预期行为。需要用 **Ctrl+Shift+Space** 打开 Launcher 后再测 |

**测试中发现的缺陷（准备步骤中触发）**：

### R2. 在 Windows 上，“新建条目”和“浏览…”只能选文件夹，选不了程序（从 M2 就存在）
- gpui 的 Windows 实现中，只要 `directories: true` 就会加上 `FOS_PICKFOLDERS`，对话框就只能选文件夹（`can_select_mixed_files_and_dirs()` 返回 `false`，源码注释也写明了“只能二选一”）。我们传的是 `files: true, directories: true`，在 Windows 上结果就是**只能选文件夹**。BoBO 实测：选 notepad.exe 时提示失败，对话框显示的是目录。
- 受影响的位置：
  1. `quick_launch/view.rs` `new_item_at`（右键空格子 → 新建条目）
  2. `quick_launch/view.rs` `browse_for_path`（编辑浮层的“浏览…”）
  3. `settings/quick_launch.rs` `browse` 中 `Field::Path` 的分支（`files: !cwd` 仍然带着 `directories: true`）
  - 不受影响：`browse` 的工作目录分支、`new_group` 选文件夹（这两处本来就只需要文件夹）。
- M2 手测第 1 项（右键空格子新建条目并选 exe）当时记为通过，应该是用拖入代替了，实际没有走到这条路径。
- **修复**：
  - 新增一个辅助函数 `path_prompt(cx, want: PathWant { Files, Folders, Either }, multiple, prompt) -> PathPromptOptions`。当 `Either` 且 `cx.can_select_mixed_files_and_dirs()` 为 false 时，改为**只选文件**（`files: true, directories: false`）。三个调用处都改用它。
  - 右键空格子的菜单：保留“新建条目”（选文件，可以多选），并在它**下面**新增“新建文件夹条目”（只选文件夹，可以多选）。新文案用 `t!("Novo item de pasta")`，中文为“新建文件夹条目”，9 种语言都要补齐。平台本身支持同时选文件和文件夹时，不显示这个新菜单项。
  - 编辑浮层和设置页的“浏览…”：条目类型是 `folder` 时只选文件夹，其他类型只选文件。
  - 选回来的路径仍然走原来的 `path_is_directory` 和 `item_for_path`，类型判断不变。
  - 测试：`path_prompt` 在支持和不支持同时选两种的情况下各返回什么（用参数注入，不依赖平台）；菜单项只在不支持时出现。

### R3. “当程序已经打开时”的第一个选项中文显示为“启用”
- 设置页第 240 行用的是 `t!("Ativar")`，这个键同时给 Everything MFT 的“启用 / 停用”按钮使用（`settings.rs:2868`），所以中文是“启用”。在这里表达的是“激活已有窗口”，读起来像一个开关。
- **修复**：这里改用已有的键 `t!("Ir para ele")`（中文“切换到它”，9 种语言已经齐全，F 节的询问框也用的是它）。**不要**修改 `Ativar` 的翻译，否则会影响 Everything 的按钮。

---

## 8. 合入条件（更新）
R1、R2、R3 修复，BoBO 重测第 8 项，并验证 R2：在 Windows 上通过“新建条目”能选中 notepad.exe，通过“新建文件夹条目”能选中文件夹。之后合入 main 并清理临时分支。
