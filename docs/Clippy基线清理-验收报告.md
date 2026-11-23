# Clippy 基线清理 — 验收报告

> 分支：`chore/clippy-baseline`（从 `main` @ `4c3d372` 起）
> 依据：`docs/Clippy基线清理-规格.md` v1.1，§2 门槛命令全部通过。
> 结论：**完成**。`cargo clippy --workspace --all-targets --release -- -D warnings` 退出码 0，`main` 上曾报错的 `nohrs-core`、`nohrs-indexer`、`nohrs/build.rs` 都已清空，下游 crate 也全部检查过了。`nohrs-cli` 的 4 个日志测试现在全部通过。

---

## 1. 第 0 步盘点

原始输出：`target\tmp\cb0-<crate>.txt`（另有一份 `.json` 用来精确取 lint 名，因为文本输出在编译器中止后偶尔丢 `-D clippy::…` 那一行）。

| crate | 条数 |
|---|---|
| nohrs-core | 14 |
| nohrs-models | 0 |
| nohrs-store | 0 |
| nohrs-services | 11 |
| nohrs-ui | 0 |
| nohrs-indexer | 5 |
| nohrs-launcher | 0 |
| nohrs-pages | 97（lib 与 lib-test 两个 target 重复报的同一位置去掉后 95：产品代码 31、测试代码 64） |
| nohrs-cli | 4 |
| nohrs（含 build.rs） | 4（全部在 `build.rs`） |
| **合计** | **135** |

按 lint 分组（step 0 时）：

| lint | 合计 | 分布 |
|---|---|---|
| `clippy::disallowed_methods`（`std::fs::read/write/read_to_string`） | 75 | pages 58（产品 37 / 测试 21）、core 11、services 4、indexer 1、nohrs build.rs 1 |
| `clippy::chunks_exact_to_as_chunks` | 6 | pages 5、indexer 1 |
| `clippy::too_many_arguments` | 5 | services 3、pages 2 |
| `clippy::cloned_ref_to_slice_refs` | 5 | services 2、pages 3 |
| `clippy::type_complexity` | 4 | pages 3、indexer 1 |
| `dead_code`（rustc） | 4 | cli 4 |
| `clippy::expect_used` | 4 | nohrs build.rs 3、pages 1 |
| `clippy::field_reassign_with_default` | 4 | pages（测试）4 |
| `clippy::collapsible_if` | 3 | pages 2、services 1 |
| `clippy::manual_is_multiple_of` | 3 | pages 3 |
| `clippy::let_and_return` | 2 | pages 2 |
| `clippy::items_after_test_module` | 2 | pages 2 |
| `clippy::if_same_then_else` | 2 | pages 2（见 §3） |
| 各 1 条的 16 个 lint | 16 | `derivable_impls`、`redundant_guards`、`while_let_loop`（core）；`needless_return`（services）；`precedence`、`manual_range_contains`（indexer）；`needless_option_as_deref`、`unnecessary_map_or`、`manual_pattern_char_comparison`、`collapsible_match`、`unnecessary_lazy_evaluations`、`clone_on_copy`、`unnecessary_sort_by`、`needless_character_iteration`、`identity_op`、`useless_vec`（pages） |

---

## 2. 产品代码里新增的 `#[allow]`

测试代码的 allow（`mod tests` 一层、`*tests.rs` 文件顶部）不在此表：按 §4.2 它们只是“测试构造 fixture，没有 UI 线程”，共 11 个文件里的一层 allow。下表是**产品代码**里的每一处。

| 文件:行 | lint | 理由 | 在哪个线程执行 |
|---|---|---|---|
| `crates/nohrs-core/src/config/paths.rs:94`（`read_portable_mode`） | `disallowed_methods` | 惰性判定便携模式，被本模块每个路径辅助函数调用 | **UI 线程**（首次可能发生在渲染/按键处理里）；已登记 §7 |
| `crates/nohrs-core/src/config/settings.rs:2860`（`read_config_table`） | `disallowed_methods` | 与紧邻的 `load_from_path` 同一条调用路径：启动时、GPUI 前台循环之前 | 启动线程（尚无窗口） |
| `crates/nohrs-services/src/fs/space.rs:647`（`place_children`） | `too_many_arguments`（8） | 递归状态的每一项各一个参数，打包成结构体只是换个地方声明 | 后台（空间图计算） |
| `crates/nohrs-services/src/search/indexer.rs:237`（`index_single_directory`） | `too_many_arguments`（9） | 每个被索引字段一个 `Field`，只是名字不同 | 后台（搜索索引线程） |
| `crates/nohrs-services/src/search/indexer.rs:266`（`index_single_file`） | `too_many_arguments`（9） | 同上 | 后台 |
| `crates/nohrs-indexer/src/client.rs:289` | `disallowed_methods` | 读提权辅助进程退出后留下的报告；本 crate 没有 GPUI | 后台 |
| `crates/nohrs-indexer/src/main.rs:36` | `disallowed_methods` | 提权进程没有界面，退出前把错误写到报告文件 | 进程主线程（无 GPUI 循环） |
| `crates/nohrs/build.rs`（文件顶部） | `disallowed_methods`、`expect_used` | 构建脚本没有 UI 线程；资源脚本写失败必须让构建失败，`OUT_DIR` 与资源编译器是构建不变量 | 构建脚本 |
| `crates/nohrs-pages/src/explorer/archive_ops.rs`（`ToolLog::read`） | `disallowed_methods` | 归档工具在后台任务里跑（`run_archive_task` spawn），退出后读它的日志 | 后台 |
| `crates/nohrs-pages/src/explorer/batch_rename/window.rs:143`（`load_list`） | `disallowed_methods` | “加载列表”按钮的点击处理 | **UI 线程**；已登记 §7 |
| `crates/nohrs-pages/src/explorer/batch_rename/window.rs:163`（`save_list`） | `disallowed_methods` | “保存列表”按钮的点击处理 | **UI 线程**；已登记 §7 |
| `crates/nohrs-pages/src/explorer/batch_rename/preview_state.rs:47` | `expect_used` | 不变量：规则模式一定带着规则（`None` 只会出现在列表模式分支） | 后台（`background_spawn`） |
| `crates/nohrs-pages/src/explorer/everything.rs`（`load`） | `disallowed_methods` | 索引快照由 `ensure` 在后台任务里加载 | 后台 |
| `crates/nohrs-pages/src/explorer/view/space_map.rs:975`（`paint_mosaic`） | `too_many_arguments`（10） | 布局、高亮、配色各一个参数，打包成结构体没有帮助 | UI 线程（绘制） |
| `crates/nohrs-pages/src/listary.rs`（`read`） | `disallowed_methods` | 设置页和欢迎页都用 `background_spawn` 读它 | 后台 |
| `crates/nohrs-pages/src/listary.rs`（`write`） | `disallowed_methods` | `set_listary` 用 `background_spawn` 调 `enable`/`disable` | 后台 |
| `crates/nohrs-pages/src/welcome.rs:261`（`config_card`） | `too_many_arguments`（8） | 卡片的每一列各一个参数 | UI 线程（界面构造） |
| `crates/nohrs-cli/tests/support/binary_under_test.rs`（`binary_under_test`、`copy_for_test`） | `dead_code` ×2 | 这个模块被两个集成测试 target 各自编译一次，每个 target 只用到其中一个函数 | 测试进程 |

---

## 3. 可疑点

第 0 步列出的两处 `if_same_then_else`，复查方在规格 §9 给出结论：**都不是 bug**，已按指定写法合并，没有用 allow。

### 3.1 `crates/nohrs-pages/src/explorer/page.rs:1881`

```rust
// 之前
let recent_heading = if !self.tab_menu_recent && self.tab_list_menu.is_some() {
    None
} else if self.recently_closed.is_empty() || self.tab_list_menu.is_some() {
    None
} else {
    Some(rows.len())
};

// 现在
let recent_heading = if self.recently_closed.is_empty() || self.tab_list_menu.is_some() {
    None
} else {
    Some(rows.len())
};
```
第一个分支被第二个完全覆盖（`tab_list_menu.is_some()` 时两边都返回 `None`），合并后行为不变。

### 3.2 `crates/nohrs-pages/src/explorer/path_edit.rs:79`

```rust
// 之前
let target = if super::network::is_unc(&typed) {
    Some(typed.clone())
} else if path.is_dir() {
    Some(typed.clone())
} else if path.is_file() { ... } else { None };

// 现在
let target = if super::network::is_unc(&typed) || path.is_dir() {
    Some(typed.clone())
} else if path.is_file() { ... } else { None };
```
`is_unc` 必须留在前面：短路后网络地址不会走到 `path.is_dir()`（否则会卡住窗口）。上方关于网络地址的注释保留，并补了一句说明顺序的原因。

`page.rs:1905` 写死的中文不在本次范围内，未改。

---

## 4. 被删掉 / 被 `cfg` 限定的 dead code

按 §4.3 先查用途：这 5 项**只在 Unix 测试里用到**，所以用 `#[cfg(unix)]` 限定，没有删除、也没有加 `allow(dead_code)`：

| 位置 | 项 | 处理 |
|---|---|---|
| `crates/nohrs-cli/src/doctor.rs:361` | `Fixture::shim_dir` | `#[cfg(unix)]`（只有 Unix 测试构造 shim） |
| `crates/nohrs-cli/src/doctor.rs:368` | `Fixture::install_shim` | `#[cfg(unix)]` |
| `crates/nohrs-cli/src/shim.rs:479` | `Fixture::system_bin_first` | `#[cfg(unix)]` |
| `crates/nohrs-cli/src/shim.rs:502` | `Run::stdout` 字段（及初始化） | `#[cfg(unix)]` |
| `crates/nohrs-cli/src/shim.rs:520` | `install_all` | `#[cfg(unix)]` |

被删掉的代码：只有 `crates/nohrs-services/tests/ledger_trash_round_trip.rs` 里 CLI 修复顺带暴露的 3 个未使用 `use`（`Command`、`ops`、`trash::{self, Store}`）——它们确实只在非 Windows 测试里用，已改为 `#[cfg(not(target_os = "windows"))]`，没有删除。

---

## 5. 门槛命令的原始输出

### `cargo fmt --all -- --check`

退出码 0，无 diff。stdout 里有 rustfmt 的既有提示（`rustfmt.toml` 的 nightly 选项被 stable 忽略），与本次改动无关：

```
Warning: can't set `imports_granularity = Crate`, unstable features are only available in nightly channel.
Warning: can't set `group_imports = StdExternalCrate`, unstable features are only available in nightly channel.
```

### `cargo clippy --workspace --all-targets --release -- -D warnings`

```
    Finished `release` profile [optimized] target(s) in 1m 05s
warning: the following packages contain code that will be rejected by a future version of Rust: proc-macro-error2 v2.0.1
note: to see what the problems were, use the option `--future-incompat-report`, or run `cargo report future-incompatibilities --id 1`
```
退出码 0。**0 个 clippy/rustc warning 或 error**；最后那条是依赖 `proc-macro-error2` 的未来兼容提示，不归本次管（§8 不允许升级依赖）。

### `cargo test --release -p nohrs-core -p nohrs-services -p nohrs-store -p nohrs-models -p nohrs-indexer -p nohrs-launcher -p nohrs-cli`

```
running 86 tests     ... test result: ok. 86 passed; 0 failed
running 4 tests      ... test result: ok. 4 passed; 0 failed        ← 4 个日志测试
running 17 tests     ... test result: ok. 16 passed; 1 ignored      ← quick_launch 集成测试
                        test result: ok. 1 passed; 0 failed
running 147 tests    ... test result: ok. 146 passed; 1 ignored     ← core
                        test result: ok. 1 passed; 0 failed
running 46 tests     ... test result: ok. 41 passed; 5 ignored      ← indexer
running 53 tests     ... test result: ok. 53 passed; 0 failed       ← launcher
running 143 tests    ... test result: ok. 139 passed; 4 ignored     ← services
running 30 tests     ... test result: ok. 30 passed; 0 failed       ← store
                        test result: ok. 1 passed; 0 failed
running 2 tests      ... test result: ok. 2 passed; 0 failed
```
**0 个失败**，退出码 0。测试数量没有减少（对比 `main`：core 146、services 139、indexer 41、launcher 53、cli 86+4+16，一致）。

### `cargo test --release -p nohrs-pages`

```
running 558 tests
test result: ok. 545 passed; 0 failed; 13 ignored
```
（`main` 上同样是 545 passed / 13 ignored。）

### `cargo build --release -p nohrs -p nohrs-cli`

```
    Finished `release` profile [optimized] target(s) in 34.60s
LumaFiles.exe  size=48,084,480  built=2026-11-23 19:12:30
noh.exe        size= 6,634,496  built=2026-11-23 19:16:26
```

---

## 6. `git diff --stat main..chore/clippy-baseline`

```
 53 files changed, 847 insertions(+), 270 deletions(-)
```

提交（每个一步）：

```
68fda2c Clear the cli's dead code and make the four log tests pass
f7b7f6b Clear the clippy findings of the nohrs-pages tests
b8b4b71 Clear the clippy findings of the nohrs-pages library
4bb593a Clear the clippy findings of the launcher and the nohrs binary
4ee7730 Clear the nohrs-indexer clippy findings
dae8ca4 Clear the clippy findings of the models, store, services and ui crates
e12e0d9 Clear the nohrs-core clippy findings
292b6da Update the clippy baseline spec to v1.1
c910528 Add the clippy baseline spec
```

---

## 7. `nohrs-cli` 集成测试的复制与日志目录（与规格 §4.4 的实际情况）

§4.4 的修法（`binary_under_test()` 挪到 `tests/support/`，两个测试文件都用 `#[path]` 引入）已经照做。实际做法：

- `tests/support/binary_under_test.rs` 的 `binary_under_test()` 每个测试进程把 `noh` 复制到 `%TEMP%` 下一个按“源文件大小 + 修改时间”命名的目录，**每个进程只复制一次**，两个测试文件共用这份副本。复制先写唯一临时文件再改名（带重试），因为杀毒软件持有刚写完的文件时 `rename` 会返回“拒绝访问”。
- 子进程一律设置 `XDG_CONFIG_HOME`、`XDG_DATA_HOME`、`XDG_STATE_HOME`、`XDG_CACHE_HOME` 指向测试自己的临时目录，并去掉继承的 `RUST_LOG`。
- **日志测试**（`tests/log_command_leaves_the_log_alone.rs`）：每个测试有自己的 state 临时目录，子进程的 `XDG_STATE_HOME` 指向它，所以不会碰到开发者真实的日志目录。日志目录按平台取：
  - **Windows**：可执行文件所在目录（`docs/logging.md`），也就是那份共享副本的目录。4 个测试因此共用一个目录，用一把 `Mutex` 轮流执行，每个测试开始时清掉上次留下的日志。
  - **其他平台**：`<state>/nohrs/logs`，每个测试一个，可以并行。
- logo 之外的 `nohrs-cli` 测试（`quick_launch.rs`）只用同样的副本和 XDG 隔离，不涉及日志目录。

**已知限制（登记）**：Windows 上 `quick_launch.rs` 的测试和日志测试共用同一个副本目录，quick_launch 的子进程会把日志写进这个目录。cargo 默认按顺序一个个跑测试二进制，加上日志测试的 `turn()` 先清理，所以现在没有问题；如果以后改用 nextest 这类并行执行器，就需要给两者分开目录。

## 8. 其余说明

- `nohrs-indexer` 的 `bin` 和 `nohrs` 的 `bin` 只有在 lib 编译干净之后才会被 lint 到：这次因此多修了 `indexer/src/main.rs` 的 1 条、`nohrs/src/app.rs` 2 条、`nohrs/src/cli.rs` 1 条 —— 它们都在第 0 步的盘点之外，属于“以前从没被检查过”的部分。
- 规格 §7 已按实际执行线程填好（3 行）。
- 没有改 `clippy.toml`、`[workspace.lints]`、`rust-toolchain.toml`，没有加 `#![allow(clippy::all)]`，没有升级依赖，没有碰 `third_party/gpui`。


---

## 复查第 1 轮（复查方，2026-11-23，HEAD 851c3b4）

复查方式：直接读 git 对象，逐个提交看 diff（c910528…851c3b4，共 10 个提交）；另用脚本检查每个新增的 `allow(clippy::disallowed_methods)`，确认它管辖的范围里确实调用了 `std::fs::read`、`write` 或 `read_to_string`。

**通过的部分**：A–E 步的修改都是等价改写，行为不变（逐处核对过 `network.rs` 里 match guard 的改法：`connect_interactive` 返回 true 时改走 `_ => {}`，和原来一样什么都不做；`settings.rs` 里的 `&& let` 只在条件为真时才执行 `take()`；`precedence` 只加了括号）。两个可疑点按 §9 的写法合并，没有加 allow。dead code 用 `#[cfg(unix)]` 限定。测试数量不变（pages 改动的文件里 `#[test]` 修改前后都是 91 个），没有新增 `#[ignore]`，也没有新增 `mod.rs`。

**必须修改**：

| # | 问题 | 要求 |
|---|---|---|
| R1 | **日志测试在非 Windows 平台上会操作开发者真实的日志。** 新代码去掉了 `XDG_STATE_HOME`，又默认“日志目录 = 可执行文件所在目录”，可这只在 Windows 上成立（`paths::log_dir()` 只在 `#[cfg(windows)]` 下才返回 exe 目录）。在 Linux/macOS 上，`noh log clear --force` 会清空 `~/.local/state/nohrs/logs`，而且 4 个测试都会失败（CI 跑 Linux） | 子进程**始终**设置 `XDG_STATE_HOME=<本文件专用的临时目录>`。测试里的 `log_dir()` 按平台区分：Windows 用副本所在目录，其他平台用 `<state>/nohrs/logs`。只有 Windows 需要 `Mutex` 轮流执行 |
| R2 | **代码、注释、文档三方说法不一致。** 实际上日志测试用的是共享的 `binary_under_test()` 目录加 `Mutex`，`copy_for_test()` 根本没被调用：它是真正的 dead code，被 `#[allow(dead_code)]` 掩盖了，而且那条注释是错的。`binary_under_test()` 上那条“log tests only need copy_into”的注释也是错的。`copy_into` 仍然会重命名，和报告 §7、`docs/testing.md` §7 第 4/5 条写的“每个测试单独复制、不重命名”对不上 | 删掉 `copy_for_test()`；去掉两处 `#[allow(dead_code)]`（两个测试 target 都会用到 `binary_under_test()`，不再需要）；按代码的**实际做法**重写报告 §7 和 `docs/testing.md` §7 第 4/5 条 |
| R3 | **有 6 个新增的 `allow(clippy::disallowed_methods)` 管辖范围里没有任何文件读写**，说明当时并没有看清楚 clippy 报的位置 | 删掉这 6 个（连同前面那行注释）：`everything.rs:56`（`search_tests` 模块）、`:1119`（`push`）、`:1772`（`index_status_poll_tests`）、`:2819`（代码块）、`navigation.rs:76`（`LISTING_WAIT` 常量）、`:686`（`OPENED_WITH_DEFAULT_APP`）。验证方法：把本分支**所有新增**的 `allow(clippy::disallowed_methods)` 临时改成 `expect(...)`，跑一遍 §2 的 clippy，凡是报 `unfulfilled lint expectation` 的都删掉，然后把留下的改回 `allow`（`expect` 在某些平台上可能不满足，所以不保留） |
| R4 | **`settings.rs` 里 `read_config_table` 的理由写错了。** 它不只在启动时调用：配置热重载（`nohrs-pages/src/root.rs:672`，位于 `this.update_in` 里）也会在 **UI 线程**上调用 `load_from_files` → `read_config_table` | 改正注释（启动时调用一次，之后每次配置文件变化都在 UI 线程上再调用），并把它补登到规格 §7 表格和报告 §2 表格（线程一栏标“UI 线程”） |

**这次不改，但需要登记**：
- 在 Windows 上，`quick_launch.rs` 的测试和日志测试共用同一个副本目录，quick_launch 的子进程会把日志写进这个目录。cargo 默认按顺序一个个跑测试二进制，加上 `turn()` 会先清理，所以现在没有问题。但如果以后改用 nextest 这类并行执行器，就需要给两者分开目录。把这一点写进 `docs/testing.md` §7 第 5 条。

**R1–R4 都改完后**：重跑 §2 的全部门槛命令；另外在 cli 测试里确认 4 个日志测试仍然通过，并且对 `log_command_leaves_the_log_alone.rs` 单独跑一次 `cargo clippy -p nohrs-cli --tests --no-deps`。
