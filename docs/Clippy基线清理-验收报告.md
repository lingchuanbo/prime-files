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

## 7. 与规格 §4.4 的一处偏差（需要知道）

§4.4 的修法（把 `binary_under_test()` 挪到 `tests/support/`，两个测试文件都用它）已经照做，但**只做这一步 4 个日志测试仍然失败**：Windows 上 `log_dir()` 不是 `$XDG_STATE_HOME/nohrs/logs`，而是**可执行文件所在目录**（`docs/logging.md` 明确如此，`paths::log_dir()` 的 Windows 分支）。所以现在的做法是：

- `binary_under_test()` 仍是每个进程只拷一次，`quick_launch.rs` 用它；
- 日志测试用同一个模块里的 `copy_for_test()` 复制到自己的目录，并把这个目录当作日志目录（Windows 上二者本来就是同一个）；
- 4 个日志测试共享这一份副本，用一把 `Mutex` 轮流执行，每个测试开始时清掉上次留下的日志；
- 另外把 `fs::copy` 改成“先写唯一文件名、不重命名”，因为杀毒软件持有刚写完的文件时 `rename` 会一直返回“拒绝访问”（这正是 M3/M4 里反复出现的那类偶发失败）。

`docs/testing.md` §7 第 4、5 条已按这个实际情况改写。

---

## 8. 其余说明

- `nohrs-indexer` 的 `bin` 和 `nohrs` 的 `bin` 只有在 lib 编译干净之后才会被 lint 到：这次因此多修了 `indexer/src/main.rs` 的 1 条、`nohrs/src/app.rs` 2 条、`nohrs/src/cli.rs` 1 条 —— 它们都在第 0 步的盘点之外，属于“以前从没被检查过”的部分。
- 规格 §7 已按实际执行线程填好（3 行）。
- 没有改 `clippy.toml`、`[workspace.lints]`、`rust-toolchain.toml`，没有加 `#![allow(clippy::all)]`，没有升级依赖，没有碰 `third_party/gpui`。
