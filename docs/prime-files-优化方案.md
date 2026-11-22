# prime-files 性能与稳定性优化方案

> 范围：`D:\BoBO\prime-files`（Rust workspace，gpui 界面 + `nohrs-indexer` Windows 服务，约 226 个 `.rs` 文件、11.7 万行）
> 依据：静态扫描 `crates/`、`Cargo.toml`、`docs/PERFORMANCE.md`、`docs/windows-filename-index.md`。**本方案未编译或运行程序**，标“需测量”的项目要先跑基线再动手。

---

## 0. 现状判断（已经做得好的，不要重复做）

`docs/PERFORMANCE.md` 已记录 8 轮优化：列表改用 `DirEntry::metadata`（35k 项 2.6s→95ms）、读取移出 UI 线程 + 代次丢弃、`LCMapStringW` 排序键、同盘移动走 rename、回收站批量、`CopyFileExW`、reload 区分同目录、Shell 缩略图。索引服务已有 MFT 基线 + USN 增量 + 不可变快照替换 + 失败兜底重建。

结论：**热点路径已调过一轮，下一步收益主要来自：发布构建配置、阻塞调用清理、崩溃可观测性、大文件拆分和回归基准。**

---

## 1. 构建配置（收益高、风险低，P0）

当前：`[profile.release]` 为开发调过（`incremental = true`、`codegen-units = 256`），`[profile.dist]` 只改成 `codegen-units = 16`，**没有开 LTO**。

建议 `dist`（安装包用）：

```toml
[profile.dist]
inherits      = "release"
incremental   = false
codegen-units = 1
lto           = "fat"        # docs/size.md 已测：39 MB → 23.5 MB
panic         = "unwind"     # 保留 unwind，配合第 3 节的崩溃捕获；不要设 abort
debug         = "line-tables-only"  # 崩溃时有行号，体积增加很少
strip         = "debuginfo"  # 或单独产出 PDB 归档
```

- 可选：对 `nohrs-pages`、`nohrs-indexer` 做 PGO（`cargo pgo`），用“打开 System32 / WinSxS、滚动、搜索”作训练集。
- `iniciar-nohrs.bat` 日常仍用 `release`，安装器（`installer/prime-files.iss`）确认用 `--profile dist` 产物。
- 验证：`listing_ab`、`sort_timing`、启动时间三项 A/B，各 7 轮取中位数（沿用 PERFORMANCE.md 的方法）。

## 2. 阻塞调用与 UI 卡顿（P0–P1）

生产代码中的 `thread::sleep` 分布：

| 文件 | 次数 | 处理 |
|---|---:|---|
| `nohrs-pages/src/dialog_jump.rs` | 11 | 最高优先。确认是否在 UI/消息线程轮询窗口；改为 WinEvent 钩子（`SetWinEventHook`）或带超时的后台任务 + 回调 |
| `reveal.rs` | 4 | 改为等待读取完成的事件/通道，不用固定睡眠 |
| `file_dialog.rs`、`foreground.rs` | 2 / 2 | 同上，设总超时上限 |
| `root.rs`、`shell_windows.rs`、`listary.rs`、`explorer/status.rs`、`media.rs`、`archive_ops.rs` | 各 1 | 逐个确认所在线程；UI 线程上的一律移走 |
| `nohrs-indexer/src/client.rs`、`install.rs` | 各 1 | 重试应改指数退避 + 上限 |

其他：
- **网络/休眠盘**：`LISTING_WAIT=30ms` 机制很好，把同样的“超时 + 代次丢弃”推广到预览（`view/preview.rs`）、属性读取、压缩包浏览（`archive.rs`）。
- **Mutex**：57 处，`explorer/everything.rs` 占 18 处。检查是否有锁跨越 IPC/磁盘 I/O；读多写少的状态改 `RwLock` 或 `arc-swap` 快照（与索引服务同思路）。
- `.clone()` 共 1188 处（`pane_group.rs` 57、`page.rs` 48）：只处理性能剖析里出现在渲染路径上的；大列表数据改 `Arc<[T]>` / `SharedString`。

## 3. 稳定性：崩溃捕获与恢复（P0）

扫描结果：
- GUI 主入口 `crates/nohrs/src/main.rs` **没有** `panic::set_hook` / `catch_unwind`；只有 `nohrs-core/telemetry/logging.rs` 和 `nohrs-indexer/service.rs` 有。需确认 logging 里的 hook 是否在 GUI 启动时最早安装。
- 代码中 `unsafe` 块 531 处（`dialog_jump.rs` 122、`everything.rs` 56、`shell_menu.rs` 49、`media.rs` 36），是 Win32/COM 崩溃的主要风险区。

建议：
1. **最早安装 panic hook**：写 JSONL 日志 + 当前路径/操作，然后弹“出错了，日志位于…”。
2. **原生崩溃**：`SetUnhandledExceptionFilter` 写 minidump（`minidumper`/`crash-handler` crate），配合 dist 的行号信息。
3. **隔离高风险操作**：Shell 右键菜单、缩略图、第三方预览处理器这类 COM 调用在 `catch_unwind` 内执行；可能被第三方扩展拖崩的（shell 扩展），考虑放到子进程。
4. **unsafe 收敛**：每个模块收成少量带 `// SAFETY:` 注释的安全封装；COM 对象统一用 RAII（`windows` crate 的智能指针），杜绝手动 `Release`。HANDLE 用 `OwnedHandle`。
5. **会话恢复**：定期保存标签页/窗格状态（已有 `nohrs-store`），崩溃后重启恢复。

## 4. 错误处理（P1）

- 生产代码 `.unwrap()` 约 48 处（排除 tests/benches/examples），`.expect(` 约 291 处（含测试），`panic!/todo!/unreachable!` 20 处。
- 已开 `clippy::unwrap_used = "warn"`：CI 用 `-D warnings` 时把它对**非测试代码**升为 deny，测试里 `#[allow]`。
- `launcher/field.rs`（23 个 expect）、`archive.rs`（21）、`archive_ops.rs`（19）逐个审查：凡依赖外部输入（文件名、压缩包内容、注册表、IPC 数据）的一律改 `?` + 用户可见的错误。
- 压缩包：加防护（解压路径穿越、超大条目、zip bomb 的总量上限）。

## 5. 索引服务（P1–P2）

`docs/windows-filename-index.md` 已写明的边界：
- **重启后全量重扫**（不保存 USN 游标/MFT 基线）。建议持久化快照 + 最后 USN 到磁盘（用已有的 redb），启动时校验日志 ID 后只回放增量；首次快照前的搜索等待随之消失。
- **快照构建仍遍历全部名称**（D 盘 189 万条约 3.25 s 基线）。改为分段/分卷快照，增量只重建受影响的段。
- 轮询 2 s：可改为 `FSCTL_READ_USN_JOURNAL` 带 `BytesToWaitFor` 的阻塞读，降低延迟和空转。
- 记得这次修过的协议 bug（`u16` 写成 `usize`）：为命名管道协议加 fuzz（`cargo fuzz`）和版本协商测试。
- 服务进程：设内存上限监控、看门狗（SCM 恢复选项：失败后重启）。

## 6. 代码结构（P2，减小改动风险）

超大文件，修改时易引入回归，编译也慢：

| 文件 | 行数 |
|---|---:|
| `nohrs-pages/src/explorer/everything.rs` | 5639 |
| `nohrs-pages/src/explorer/tests.rs` | 3861 |
| `nohrs-core/src/config/settings.rs` | 3571 |
| `nohrs-pages/src/settings.rs` | 3508 |
| `nohrs-pages/src/explorer/page.rs` | 3076 |
| `nohrs-services/src/fs/ops.rs` | 2987 |

按职责拆子模块（如 everything → ipc / query / results / ui）。`i18n/extra.rs`（7059 行）可改为 `include!` 生成或数据文件，减少 `nohrs-core` 重编译。

## 7. 防回归：测量与 CI（P1）

- 已有 criterion 和 `listing_ab` 示例：把关键基准（列表、排序、过滤、索引查询、启动）纳入 CI，超过基线 10% 报警。
- 启动阶段打点（tracing span），`noh perf` 已能聚合日志，补“冷启动到首帧”“首次列表可见”两个指标。
- Windows CI：目前 `default-members` 只覆盖无 GUI 的 crate；加一个 Windows runner 跑 `--workspace` 的 build + clippy + test（`--test-threads=1` 的那组）。
- 长时间压测：脚本反复开大目录、拖放、复制取消、拔网络盘，跑数小时看内存和句柄数（`GetProcessHandleCount`）是否增长。


## 执行顺序

| 阶段 | 内容 | 预估 |
|---|---|---|
| 第 1 周 | §1 dist 配置 + A/B；§3 panic hook + minidump；§2 `dialog_jump.rs`/`reveal.rs` 的睡眠 | 2–4 天 |
| 第 2 周 | §4 生产代码 unwrap 清零 + clippy deny；§7 Windows CI + 基准门槛 | 3–5 天 |
| 第 3–4 周 | §5 索引快照持久化 + 阻塞式 USN 读；§2 锁审查 | 1–2 周 |
| 持续 | §6 拆分大文件；§3 unsafe 收敛；§8 清理 | 按需 |

每项改动沿用 `PERFORMANCE.md` 的格式记录：症状 → 基线 → 改动 → 前后数据。

---

## 执行情况（2026-11-22 本轮）

### 已做（已编译、测试通过）

- **§1 `[profile.dist]`**：加上 `lto = "fat"`、`codegen-units = 1`、`debug = "line-tables-only"`。实测 `LumaFiles.exe` 由 release 的 43.2 MB 降到 **35.7 MB（-17%）**，`dist` 构建约 10 分钟，只在出安装包时付这个代价；`release` 日常构建不变。数据记入 `docs/size.md`。
- **§2 线程内 `sleep` 排查**（30 处生产代码）：28 处在自有消息循环 / 后台线程上（`reveal`、文件对话框 watcher、`dialog_jump`、`foreground`、`archive_ops`、`media`、`shell_windows`、索引服务等），睡眠属正常。真正在 UI 线程的 2 处已改：
  - `root.rs` 索引未就绪重试的 250 ms（最多 10 次 ≈ 2.5 s 冻结）→ 改为 `background_executor().timer()` 等待；
  - `file_dialog::fill_and_open` 多选交付时的 300 ms（Shell 异步切目录）→ 移到 `cx.background_spawn`。
- **§3 崩溃可观测性**：`panic.log` 原先只在 `diagnostics.runtime_log = true` 时安装 hook，而默认关闭、release 无控制台 → 崩溃无任何痕迹。现在 `install_panic_log` 在 `NohrsApp::run` 最开头安装（读配置之前），并记录工作目录；重复调用不会重复装 hook。记录见 `docs/PERFORMANCE.md` §9。

### 未做（原因）

- **§1 PGO**：需要 `cargo-pgo` 与训练集，收益须先测；放在 dist 配置稳定之后。
- **§2 预览 / 属性读取 / 压缩包浏览的“超时 + 代次”、Mutex 与 clone 审查**：需先有性能剖析基线，静态判断容易误伤；建议先按 §7 加启动与列表基准。
- **§3 minidump、COM 调用隔离到子进程、`unsafe` 收敛**：面大、需逐模块设计（建议单独 issue / PR）。
- **§4 生产 `unwrap/expect` 清零 + CI `-D warnings`**：当前 `--workspace --lib --bins` 仍有 **51 条既有 clippy 告警**（多为 `disallowed_methods` 用在 build/CLI、`too_many_arguments`、`chunks_exact`、可折叠 `if` 等），清完后才能开严格门禁。
- **§5 索引服务**：已确认现状确实是“重启后全量重扫”（`crates/nohrs-indexer` 没有把 USN 游标 / 快照落盘），持久化 + 分段快照 + 阻塞式 USN 读是一个独立的大任务。
- **§6 拆分超大文件**：纯结构调整，建议单独 PR。
- **§7 CI / 基准门槛**：`docs/testing.md` §2/§3 已有完整的 `ci.yml` 设计（test split、coverage、clippy `-D warnings`、cargo-deny），但仓库里只有 `release.yml`；且当前 `cargo fmt --check` 与 clippy 都不干净，严格门禁会立即变红。建议先做 §4 的清理，再照文档落地 `ci.yml`（本轮没有添加一个注定报红的 CI）。
