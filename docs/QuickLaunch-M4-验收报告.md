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


---

# 复查（第 2 轮，2026-11-23 17:05）

> 审查对象：883805b（R1）、2fc14c9、e4d5e2e（R2）、713c30f（R3）、5c44316（报告）。
> 报告恢复情况：执行方用 `git reset --hard` 丢掉了工作区中未提交的报告，之后按原文重新写入。我把它与我写入的原文逐段比对：第 7 节、R2、R3、第 8 节的内容一致，只是在第 8 节前多了一条 `---` 分隔线、空行数量不同，**没有内容缺失**。今后执行方**禁止**对含有未提交改动的工作区使用 `reset --hard`、`checkout -- .`、`stash drop` 等会丢弃改动的命令（见第 D 节）。
> 结论：R1、R3 通过；**R2 引入了一个会导致崩溃的问题（R4），必须先修**。

## A. 逐项确认

| 项 | 结果 | 依据 |
|---|---|---|
| R1 | ✅ | `group_command_overlay` 已改用 `crate::terminal::code_font()` |
| R2 快速启动窗口部分 | ✅ | 新增 `path_prompt` / `path_prompt_for` / `folder_entry_offered`，用注入参数做了测试；`new_item_at` 用 `Either`，在 Windows 上即只选文件；“新建文件夹条目”只在平台不能混选时出现（`if !folder_entry_offered(..) { menu } else { menu.item(..) }` 是双重否定，结论正确，但不好读，见 S1）；`browse_for_path` 按条目类型选择 |
| R2 设置页部分 | ❌ 见 R4 | |
| R3 | ✅ | 第 240 行改为 `t!("Ir para ele")`，`Ativar` 未改动 |
| 翻译 | ✅ | `Novo item de pasta` 和 `Escolher uma pasta` 的 9 种语言都已补齐 |

## B. 必须修

- **R4 设置页点击“浏览…”会让程序崩溃（R2 引入）。** `settings/quick_launch.rs` 的 `browse()` 新增了 `cx.entity().read(cx)` 来读取表单类型。可是 `browse` 是在 `cx.listener(move |_page, _event, window, cx| browse(field, window, cx))` 里调用的，这时 `SettingsPage` 正在被 gpui 更新（处于借出状态）。`EntityMap::read` 找不到这个实体，就会执行 `double_lease_panic("read")`，**直接 panic**。也就是说，在设置页条目表单里点“路径”或“工作目录”旁边的“浏览…”，程序就会崩溃。
  - **修复**：listener 改为 `move |page, _event, window, cx| browse(page, field, window, cx)`，`browse` 增加 `page: &SettingsPage` 参数，从 `page.quick_launch.form` 读取类型，删掉 `cx.entity().read(cx)`。
  - **测试**：在 `#[gpui::test]` 里构造一个 SettingsPage，表单类型分别为 `folder` 和 `app`，通过 listener 路径（或者在 `entity.update` 闭包内）调用 `browse`，断言不会 panic。如果 prompt 在测试平台上无法创建，就把选择逻辑抽成纯函数 `browse_want(field, kind) -> PathWant` 来测，同时保证 `browse` 内部不再读取实体本身。
  - 另外，在整个 `nohrs-pages` 里搜索 `cx.entity().read(cx)` 和 `cx.entity().update(`，确认没有其他在 listener 或 update 内部读取自身实体的地方，结果写进 PR 描述。

## C. 建议（随 R4 一起改）

- **S1** 菜单里那段双重否定改成正面写法：`let menu = menu.item(新建条目); if super::folder_entry_offered(..) { menu.item(新建文件夹条目) } else { menu }`。
- **S2** 设置页在 `Files` 模式下，提示文案仍然是 `Escolher um arquivo ou uma pasta`（“选择文件或文件夹”）。这段文字会显示为对话框的确定按钮，改成已有的 `Escolher um caminho`（“选择路径”），和编辑浮层保持一致。

## D. 执行方操作约束（新增，长期有效）

- 工作区里有**任何未提交的改动**（包括 BoBO 或复查方写入的文档）时，禁止使用 `git reset --hard`、`git checkout -- <路径>`、`git restore`（会丢弃改动的用法）、`git clean -f`、`git stash drop`。需要重写提交历史时，先 `git stash push -u`，或者先把这些改动单独提交。
- 临时备份分支按 `backup/<里程碑>-<说明>` 命名，合入后统一删除。

## E. 重测（R4 修复后，BoBO 执行）

1. **Launcher 的 `@`**：按 **Ctrl+Shift+Space**，只输入 `@`：
   - 能列出所有快速启动条目，包括文件夹分组里的程序；
   - 计算器、charmap 只出现一次；
   - 输入 `@char` 回车，打开字符映射表。
2. **新建条目**：快速启动窗口里右键空格子 → **新建条目**，能选中 `C:\Windows\System32\notepad.exe` 并成功添加。
3. **新建文件夹条目**：右键空格子 → **新建文件夹条目**，能选中一个文件夹并添加为文件夹类型。
4. **设置页的“浏览…”（验证 R4）**：设置 → 快速启动 → 条目 → 选一个**应用**类型的条目 → 点“路径”旁的“浏览…”，**程序不崩溃**，能选中文件。再选一个**文件夹**类型的条目，点“浏览…”，能选中文件夹。最后点“工作目录”旁的“浏览…”，能选中文件夹。

## F. 合入条件
R4 修好（S1、S2 一起改），并且 E 节 4 项重测全部通过，之后合入 main，清理临时分支（包括 `m4-before-r2-docfix`）。


---

# 复查（第 3 轮，2026-11-23 17:20）

> 审查对象：6d3f405（提交第 2 轮报告）、8a3fc71（R4）、cf37f03（S1）、043ef4c（S2）。工作区与 HEAD 一致，第 2 轮报告已按原文提交，没有删减。
> 结论：**代码通过**。等 BoBO 用 17:08 编译的版本完成第 2 轮 E 节的 4 项重测后，即可合入。

| 项 | 结果 | 依据 |
|---|---|---|
| R4 | ✅ | listener 改为 `move \|page, _event, window, cx\| browse(page, field, window, cx)`；`browse` 从传入的 `page` 读取表单类型，不再调用 `cx.entity().read(cx)`；`browse_want` 纯函数测试了 6 种情况；`#[gpui::test]` 在 `page.update` 内部读取，不会 panic。`nohrs-pages` 全量搜索：`cx.entity().read(cx)` / `.update(` 为 0 处；其余 27 处 `.entity()` 是 `downgrade`、在绘制闭包里使用或交给子组件，经执行方逐处核对，接受 |
| S1 | ✅ | 改为正面写法：`if folder_entry_offered(..) { menu.item(新建文件夹条目) } else { menu }` |
| S2 | ✅ | 只选文件时的提示文字改为 `Escolher um caminho` |
| 验证 | ✅ | fmt 无输出；逐包 `--no-deps` clippy 与 main 一致；`cargo build` 无 rustc 警告；`nohrs-pages` 545 个测试通过 |

**备注**：R4 的 gpui 测试只覆盖了“在 update 内部读取 page”这一形态，没有真正弹出文件对话框（测试平台上做不到）。崩溃是否已经修复，以 BoBO 重测第 4 项为准。


## 第 3 轮重测结果（BoBO，2026-11-23 17:33，使用 17:08 编译的版本）

| # | 项目 | 结果 |
|---|---|---|
| 1 | Launcher 的 `@` | ✅ |
| 2 | 新建条目可以选到 notepad.exe | ✅ |
| 3 | 新建文件夹条目 | ✅ |
| 4 | 设置页三个“浏览…” | ✅ 不再崩溃，文件和文件夹都能选。BoBO 另外反馈：**对话框没有自动跳到对应位置，下方也没有出现跳转条** |

**对第 4 项反馈的分析（不是 M4 引入的问题）**：
- BoBO 更正：说的不是搜索框，而是**对话框下方的跳转条**。也就是 LumaFiles 自带的“文件对话框跳转栏”（`dialog_jump.rs`）：在其他程序的打开/保存对话框下方贴一条栏，里面有 LumaFiles 打开的文件夹、常用文件夹和 Ctrl+G 提示（栏内还带一个搜索框）；新对话框打开时还会自动跳到 LumaFiles 当前所在的文件夹一次。
- `dialog_jump::process_name()` 遇到**本进程**的对话框会返回 `None`（`if pid == std::process::id() { return None; }`），`is_file_dialog()` 因此返回 false，所以 **LumaFiles 自己弹出的对话框一律不显示跳转栏，也不会自动跳转**。这段代码在 M1 之前（eac7738）就已存在，原因没有注明。
- gpui 的 `PathPromptOptions` 没有“初始目录”字段，所以快速启动的“浏览…”打不开在条目当前所在的文件夹，只能落在 Windows 记住的上一次位置。
- 结论：第 4 项按 M4 的验收标准**通过**；“自己的对话框也要有跳转栏，并且自动定位到条目所在的文件夹”是新需求，处理方式由 BoBO 决定（见下一节）。

- **BoBO 决定（17:38）**：先合入 M4；跳转条单独作为 **M4.1** 处理，规格另写。

## M4 结论
**验收通过，可以合入 main。** 第 2 轮 E 节的 4 项重测全部通过；第 4 项附带提出的跳转条需求转到 M4.1 处理。


---

## 补审：Quick Launch UI 合并（`d9fe342`，`feature/quick-launch-ui` 的 8 个提交，复查方 2026-11-24）

按 BoBO 的决定，在内存优化第 1 轮回报时一起补审。看的是 8 个提交合在一起的总改动（10 个文件）。

**改了什么**：网格的列数和行数改为按窗口大小和 `cell_size` 自动计算，格子会拉伸到填满窗口，间距 5 px；列表的列数也按窗口宽度自动计算（最多 8 列）；设置页去掉了“列数”“列表列数”，新增“显示格子”（`show_cells`，默认开）；去掉了 96 px 图标档；默认改为 `cell_size = 100`、`icon_size = 64`；鼠标悬停时图标放大 1.2 倍（120 ms 动画）；打开窗口、切换分组时不再自动选中第一个条目；弹出窗口加了可拖动调整大小的边框（`WS_THICKFRAME`）。

**结论：没有发现 bug，可以保留。** 核对过的点：
- schema JSON 合法；新增的翻译键 `Mostrar células` 在 `strings.rs`、`extra.rs` 里都有；旧文件中的 `columns`、`list_columns` 仍然能解析（因为 `deny_unknown_fields`，这两个字段必须保留，现在只是不再使用），`icon_size = 96` 会被吸附到 64。
- 悬停动画结束后就不再请求新帧，不会在空闲时一直占用 CPU 或 GPU。
- `grid_icon_size` 用的是 `f32::clamp(16, cell_height − 30)`，`cell_height` 小于 46 时会 panic。不过窗口最小尺寸是 360×300，减去标题栏等界面元素后，格子高度至少约 59，所以目前不会触发。

**顺手修（随内存优化下一轮一起提交）**：
1. `grid_icon_size` 改成 `.min(上限).max(16.0)`，即使窗口最小尺寸的保证将来失效，也不会 panic。
2. `settings/quick_launch.rs` 的 `enum Action` 里，删掉 `Columns`、`ListColumns` 后留下了两行孤立的文档注释（“A new grid column count.”、“A new list column count.”），现在会被挂到下一个变体上，删掉这两行。
3. 规格 `QuickDock-技术实现方案与边界约定.md` 补一条 v2.9 记录：§12.1 网格和列表的列数改为自动计算；§12.2 打开窗口和切换分组时不再自动选中条目，只有在过滤框打字时才选中第一个匹配项。
