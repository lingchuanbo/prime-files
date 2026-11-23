# Clippy 基线清理 — 规格与边界约定 v1.1

> 起点：`main` @ `4c3d372`（Merge Quick Launch M4）
> 分支：`chore/clippy-baseline`
> 分工：复查方写规格并复查，DeepSeek 写代码。

## 1. 为什么做

- 现在 `cargo clippy --workspace` 在 `nohrs-core`、`nohrs-indexer`、`nohrs/build.rs` 上就报 error 停下了，**下游 crate 根本没被检查**。M1–M4 只能逐包加 `--no-deps`，再和 `main` 比条数，这只能保证“没有变多”，不能当合入门槛。
- 目标：一条命令输出 0 条，以后每个里程碑都用它把关。
- 顺带修掉 `nohrs-cli` 的 4 个日志测试：它们在开发机上因为便携模式而失败（`docs/testing.md` §7 第 4 条）。

## 2. 完成标准（门槛命令）

在开发机（Windows，便携模式的 `target\release\userdata` **保持原样，不要挪走**）上：

```powershell
cargo fmt --all -- --check                                          # 无输出
cargo clippy --workspace --all-targets --release -- -D warnings     # 0 个 warning / error，退出码 0
cargo test --release -p nohrs-core -p nohrs-services -p nohrs-store -p nohrs-models -p nohrs-indexer -p nohrs-launcher -p nohrs-cli
cargo test --release -p nohrs-pages                                 # 545 个（或以上）全部通过
cargo build --release -p nohrs -p nohrs-cli
```

- `nohrs-cli` 的测试**全部通过，包括 4 个日志测试**。
- 测试数量不能比 `main` 少：删掉、`#[ignore]` 或用 cfg 关掉测试，都不算通过。

## 3. 已知基线（复查方根据 DeepSeek 在 M4 留下的 `target\tmp\m4z-branch-*.txt` 统计）

| crate | 条数 | 说明 |
|---|---|---|
| nohrs-core | 14 | 11 条禁用的 `std::fs::*`（`config/storage.rs`、`paths.rs`、`settings.rs`），另有 `derivable_impls`、`redundant_guards`、`while_let_loop` 各 1 条 |
| nohrs-services | 11 | `std::fs::write` 4 条，`too_many_arguments` 3 条，`cloned_ref_to_slice_refs` 2 条，`collapsible_if`、`needless_return` 各 1 条 |
| nohrs-pages | 97 | 约 60 条是测试代码里的 `std::fs::*`（`batch_rename/tests.rs` 21 条、`everything.rs` 16 条……），其余是各种机械性 lint |
| nohrs-cli | 4 | `shim.rs`、`doctor.rs` 里有从未使用的代码 |
| nohrs-launcher | 0 | |
| nohrs-indexer、`nohrs/build.rs`、nohrs（bin）、nohrs-ui、nohrs-models、nohrs-store | **未统计** | 从来没被单独检查过，见 §5 第 0 步 |

**以第 0 步实际跑出来的结果为准，上表只用来核对。**

## 4. 修法规则

### 4.1 总原则
1. **不改行为。** 这是纯清理：不改界面、配置格式、文案、时序。
2. **优先真正修掉，而不是 `allow`。** 只有下面写明可以 `allow` 的情况才可以用；每个 `#[allow(...)]` 前面都要有一行注释说明为什么（`.rules`：注释只写为什么）。
3. 不允许 crate 级或模块级的“全部放行”（如 `#![allow(clippy::all)]`、`#![allow(warnings)]`），也不要改 `clippy.toml` 或 `[workspace.lints]` 来降低标准。
4. 不能用 `let _ =` 吞掉结果，不能新增 `unwrap`，不能加 `mod.rs`（`.rules`）。
5. 一个 lint 有好几种修法时，选 diff 最小、最容易看懂的那种。

### 4.2 禁用的同步文件读写（`std::fs::read` / `write` / `read_to_string`）
- **测试代码**（`#[cfg(test)] mod tests`、`*tests.rs`、`tests/`、bench）：按 `clippy.toml` 的约定，在 `mod tests` 一层（或测试文件顶部用 `#![allow]`）加 `#[allow(clippy::disallowed_methods)]`，理由写“测试构造 fixture，没有 UI 线程”。不要在测试里每一行都加 allow。
- **`build.rs`**：文件顶部 allow，理由写“构建脚本，没有 UI 线程”。
- **产品代码**：逐个判断这段代码跑在哪个线程上，结果填进报告表格（§6）：
  - 启动时、UI 还没出现之前就读配置，或者本来就在后台线程上：就地 `#[allow]`，理由写清楚具体在哪个线程或什么时候执行。
  - **在 UI（前台）线程上**：这次**不要**搬到后台，以免改变行为。也是就地 allow，理由写“在 UI 线程上，已登记在 docs/Clippy基线清理-规格.md §7”，并把这一条记到 §7 表格里，留给以后的性能优化。
  - `nohrs-core` 不能依赖 `nohrs-services`，所以 core 里不能改用 `services::fs`。

### 4.3 其他 lint 的约定
| lint | 做法 |
|---|---|
| `too_many_arguments` | 这次**不重构函数签名**：就地 allow，理由写“参数一一对应 X，打包成结构体对可读性没有帮助”这类具体说明 |
| `type_complexity` | 加一个 `type` 别名 |
| `if_same_then_else`（“`if` 的两个分支一样”） | **先判断是不是 bug。** 如果某个分支本来应该做别的事，**不要修**：记到报告的“可疑点”里，并就地 allow，理由写“待复查方确认”；确认只是写法问题，才合并分支 |
| `let_and_return` | 如果绑定的变量是锁的守卫（guard），或者涉及 drop 顺序，就保留原写法并 allow 说明原因；否则照建议修 |
| `expect_used`（产品代码） | 改成返回错误或使用默认值，并保持原来的效果；如果 panic 本来就是对的（不变量），allow 并写明这个不变量 |
| `dead_code`（nohrs-cli） | 先查清楚：①确实没人用就删掉；②只在某个平台或某个 target 用到，就用 `#[cfg(...)]` 限定。不要只加 `allow(dead_code)` |
| `no_effect`、`useless_vec`、`items_after_test_module` 等机械性 lint | 照 clippy 的建议修 |
| `chunks_exact` + 常量块大小 | 改用 `as_chunks` / `as_chunks_mut`（rust-version 1.95，可以用） |
| rustc 的 `missing_docs` 等 warning | 补文档注释（英文，说清楚这一项是什么） |

### 4.4 日志测试（`crates/nohrs-cli/tests/log_command_leaves_the_log_alone.rs`）
- 原因：`CARGO_BIN_EXE_noh` 指向 `target\release\noh.exe`，旁边有 `userdata`，于是进入便携模式，`XDG_STATE_HOME` 被忽略。
- 修法：沿用 `tests/quick_launch.rs` 的 `binary_under_test()`（每个进程只把 noh.exe 拷到临时目录一次）。把这个函数挪到 `crates/nohrs-cli/tests/support/binary_under_test.rs`，两个测试文件都用 `#[path = "support/binary_under_test.rs"] mod binary_under_test;` 引入（不建 `mod.rs`）。行为和 `quick_launch.rs` 现在的版本完全一致。
- 改完后，更新 `docs/testing.md` §7 第 4 条，改成“已修复，原因和修法见第 5 条”，并在第 5 条里加上日志测试。

## 5. 执行顺序（每一步一个提交，提交说明用英文）

| 步 | 内容 | 本步的检查 |
|---|---|---|
| 0 | **先盘点，不改代码**：对每个 crate 单独跑 `cargo clippy --release -p <crate> --all-targets --no-deps -- -D warnings`（10 个 crate，`nohrs` 包括 build.rs），把原始输出保存为 `target\tmp\cb0-<crate>.txt`，并在报告里列出每个 crate 的条数和按 lint 分组的统计 | 报告 §1 |
| A | nohrs-core | `-p nohrs-core --no-deps` 输出 0；core 测试通过 |
| B | nohrs-models、nohrs-store、nohrs-services、nohrs-ui | 同上 |
| C | nohrs-indexer | 同上 |
| D | nohrs-launcher、nohrs（build.rs 和 bin） | 同上 |
| E | nohrs-pages 的产品代码 | `-p nohrs-pages --lib --no-deps` 输出 0 |
| F | nohrs-pages 的测试代码 | `-p nohrs-pages --all-targets --no-deps` 输出 0；545 个测试通过 |
| G | nohrs-cli（dead code）以及日志测试（§4.4） | cli 测试全部通过 |
| H | 文档：`docs/testing.md` 里的 clippy 命令统一改成 §2 的门槛命令（Windows 开发机用 `--release`），更新 §7 第 4、5 条；本规格 §7 填好 | 跑一遍 §2 的全部命令 |

## 6. 报告要求（`docs/Clippy基线清理-验收报告.md`，由 DeepSeek 起草）
1. 第 0 步的盘点表（每个 crate 的条数，按 lint 分组）。
2. **产品代码里每一个新增的 `#[allow]`** 都列成表：文件:行、lint、理由、在哪个线程上执行。
3. 可疑点：`if_same_then_else` 等看起来像 bug 的地方，附代码片段。
4. 被删掉的 dead code 列表，附删除理由。
5. §2 门槛命令的原始输出（fmt、clippy、两组 test 的汇总行、build 的时间）。
6. `git diff --stat main..chore/clippy-baseline`。

## 7. 在 UI 线程上做同步文件读写的地方（留给性能优化）

| 文件:行 | 调用 | 触发时机 | 预计影响 |
|---|---|---|---|
| `crates/nohrs-core/src/config/paths.rs:94` | `std::fs::read_to_string` | 每次调用 `paths` 的辅助函数（`config_dir`、`data_dir`、`cache_dir`、`state_dir`、`log_dir`、`config_file` 等）时惰性执行；首次可能发生在渲染或按键处理里 | 读 `config.toml` 与 `userdata/config.toml` 两个小文件（各几十字节）来判定便携模式；高频路径上可缓存结果 |
| `crates/nohrs-pages/src/explorer/batch_rename/window.rs:143` | `std::fs::read_to_string` | 点击“加载列表”按钮 | 读用户保存的列表文件（上限 4 MiB），一次点击一次 |
| `crates/nohrs-pages/src/explorer/batch_rename/window.rs:163` | `std::fs::write` | 点击“保存列表”按钮 | 写同一个文件，一次点击一次 |

## 8. 不做的事
- 不改函数签名（`too_many_arguments` 一律 allow）。
- 不把同步文件读写搬到后台线程（只做登记）。
- 不升级依赖，不改 `rust-toolchain.toml`、`clippy.toml`、`[workspace.lints]`。
- 不碰 `third_party/gpui`。
- 不加 CI（以后另立项）。

## 9. 第 0 步盘点复查（v1.1，复查方根据 `target\tmp\cb0-*.txt`）

| crate | 条数 |
|---|---|
| nohrs-core | 14 |
| nohrs-services | 11 |
| nohrs-indexer | 5（`precedence`、`chunks_exact_to_chunks`、`type_complexity`、`manual_range_contains`、`disallowed_methods` 各 1） |
| nohrs（全部在 build.rs） | 4 |
| nohrs-pages | 97（去掉 lib 和 test 两个 target 重复报的同一位置后是 95） |
| nohrs-cli | 4（全部是 dead_code） |
| nohrs-launcher / models / store / ui | 0 |

**更正 pages 的划分**：产品代码 **31** 条，测试代码 **64** 条（`#[cfg(test)] mod tests` 内的也算测试代码，例如 `everything.rs` 5062–5639 行、`search_window.rs`、`archive_ops.rs`、`navigation.rs` 等文件里的测试模块）。E 步用 `--lib` 检查，本来就不会编译 `cfg(test)`，按这个检查分步即可。

**`precedence`（indexer）**：加括号时要**保持现在的运算顺序**（也就是写出编译器现在实际的结合方式），不要按“看起来应该是”的意思改。如果现在的顺序看起来不对，列为可疑点。

**两个可疑点的结论（都不是 bug，按下面的方式合并，行为不变）**：
1. `explorer/page.rs:1881`：第一个条件 `!tab_menu_recent && tab_list_menu.is_some()` 已经被第二个条件里的 `tab_list_menu.is_some()` 完全覆盖，是多余的分支（后面的 fly-out 判断同样要求 `tab_list_menu.is_none()`，说明“最近关闭”本来就只出现在标签页右键菜单里）。改成：
   ```rust
   let recent_heading = if self.recently_closed.is_empty() || self.tab_list_menu.is_some() {
       None
   } else {
       Some(rows.len())
   };
   ```
2. `explorer/path_edit.rs:79`：UNC 分支放在前面，是为了让网络地址**不去调用** `path.is_dir()`（上方注释写明会卡住窗口）。用短路的 `||` 合并，顺序不变：
   ```rust
   let target = if super::network::is_unc(&typed) || path.is_dir() {
   ```
   上方那段关于网络地址的注释保留，并补半句“`is_unc` 必须写在前面”。

**顺带发现，不在本次范围内**：`page.rs:1905` 的 `"最近关闭 ({})"` 是写死的中文，没有经过 `t!`，其他语言的界面也会显示中文。登记到以后的 i18n 清理，本次不改。
