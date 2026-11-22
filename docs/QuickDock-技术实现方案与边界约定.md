# 快速启动（Quick Launch）技术实现方案与边界约定 · v2.4

> v2.1（2026-11-22）：写入 Q1–Q5 的确认结果，新增 §15.0（主程序常驻）和 §15.1（常驻模式）。
> v2.2（2026-11-22，M1 验收后）：§8.4 改为启动、图标两条线程。M1 的具体问题见 `docs/QuickLaunch-M1-验收报告.md`。
> v2.3（2026-11-22，M1 复查后）：§8.5 失败提示改为窗口底部一行；§4.2 补充 BOM 的处理。
> v2.4（2026-11-22，M2 开工前）：新增 §12.3，规定 M2 的添加和编辑界面。

> 这是一份**实现规格**，执行方（DeepSeek）按它写代码。写着“必须 / 禁止”的条款不能自行改动；没写到的情况按 §19 处理。
> 需求见 `docs/LumaFiles-快速启动工具方案.md`。两份文档有冲突时，**以本文为准**。
> 仓库：`D:\BoBO\prime-files`。必须先读根目录的 `.rules`（`AGENTS.md`、`CLAUDE.md` 都指向它），以及 `clippy.toml`。

---

## 0. v2 审核记录（v1 → v2 改了什么、依据是什么）

v1 写在核对代码之前。v2 逐条对照了仓库现状，下表列出被推翻或修正的假设。**执行方不要再参照 v1。**

| # | v1 的写法 | 仓库实际情况（依据） | v2 的处理 |
|---|---|---|---|
| 1 | 模块名、文件名、配置名用 `dock` | `nohrs-pages/src/dock.rs` 已经是“侧边面板停靠”，`nohrs_core::config::DockSide` 也已存在 | 代码里统一用 **`quick_launch`**，配置文件叫 **`quicklaunch.toml`**，界面上叫“快速启动”（§2） |
| 2 | 界面放在 `nohrs-launcher` | launcher crate 没有 `windows` 依赖，也不依赖 `nohrs-pages`。隐藏窗口（`tray.rs`）、Shell 图标提取（`explorer/thumbnails.rs`）、在新标签打开（`explorer/page.rs`）、外部拖入（`ExternalPaths`）、设置页都在 `nohrs-pages` | 界面放进 **`nohrs-pages`**。launcher 只改热键模块，M3 再加搜索数据源 |
| 3 | “再注册一个热键即可” | `hotkey.rs` 用一个 `Registration` Global 持有**唯一**的 `GlobalHotKeyManager`。第二次调用 `set_global` 会替换并析构第一个，结果是 **Launcher 热键被注销**。另外两个轮询循环会从同一个进程级 `GlobalHotKeyEvent::receiver()` 里互相抢事件 | 把热键模块重构成“一个管理器、一个轮询循环、按 id 分发”（§10），现有行为和测试保持不变 |
| 4 | 窗口隐藏后常驻 | gpui Windows 平台的 `hide()` 是空实现；现有 Launcher 的做法是 `remove_window` 后重新打开。`tray.rs` 已经有用 Win32 直接显示/隐藏 HWND 的先例 | 常驻加 Win32 隐藏，复用 `tray.rs` 的底层做法，但**不能**动主窗口的托盘状态（§11） |
| 5 | redb 键 `dock.window.<monitor_id>`、`dock.usage.<group/slot>` | `KvKey` 只接受 `<namespace>.<name>`，并且只能是**小写 ASCII、数字和 `_`**。`/`、`-`、大写都会被拒绝 | 键格式重新设计（§6），分组 id 规则同步收紧 |
| 6 | i18n 用 `dock.*` 这样的键；分组名支持 `i18n:` 前缀 | i18n **以葡萄牙语原文作为键**：`t!("Copiar")`。`strings.rs` 提供 En/Es，`extra.rs` 提供 Fr/De/It/Ru/Ja/Zh。还有一个测试会扫描所有 `t!`，缺翻译就失败 | 改成葡萄牙语原文加 9 种语言的行（§14）。去掉 `i18n:` 前缀，用户填的分组名原样显示 |
| 7 | 用 `fs::write` 写文件 | `clippy.toml` 禁止 `std::fs::read/write/read_to_string`（防止阻塞 UI 线程），例外必须写 `#[allow(clippy::disallowed_methods)]` 并说明原因 | 读写放在后台线程，加 allow 并注明原因（§5） |
| 8 | 所有 Win32 代码直接写在 `nohrs-services` | `nohrs-services` 属于 `default-members`，**Linux CI 会编译它**。它的 `windows` 依赖放在 `[target.'cfg(windows)']` 下 | services 里的 Win32 代码一律加 `#[cfg(windows)]`，非 Windows 平台提供返回 `Unsupported` 的桩函数（§8） |
| 9 | 每个文件顶部写 `#![allow(unsafe_code)]` | 仓库主流做法是**逐项** `#[allow(unsafe_code)]`（39 处），模块级的只有 5 处 | 逐项 allow，每处写 `// SAFETY:`（§8.4） |
| 10 | 用 `launch/mod.rs` 拆成很多小文件 | `.rules`：禁止 `mod.rs`；“尽量放进已有文件，避免很多小文件” | 文件数压到最少（§3） |
| 11 | 自己写图标提取 | `explorer/thumbnails.rs` 已有 `extract(path, size, mode)` 和 `init_thread()`（基于 `IShellItemImageFactory`） | 复用，不重写（§9） |
| 12 | 自己封装“忽略错误” | `nohrs_core::telemetry::LogErr::log_err()` 已存在；`.rules` 禁止 `let _ =` 吞掉错误 | 必须用 `.log_err()` |
| 13 | 性能指标“热键到可见 ≤ 50 ms、空闲 CPU 0%” | 现有热键本来就是 **40 ms 轮询**（`POLL_INTERVAL`），延迟已经最多 40 ms，进程也不是零唤醒 | 指标改为从“收到召唤”开始计时，并约定“不新增定时器”（§16） |
| 14 | 用 `ConfigWatcher` 监听即可 | `ConfigWatcher` 会额外放行同目录的 `appearance.toml`、`search.toml` 事件 | 重载前先比较文件内容哈希，没变就跳过（§5.3） |
| 15 | 配置模型用 `deny_unknown_fields` | 主配置 `Config` 没用这个属性 | 本文件仍然**保留** `deny_unknown_fields`，因为它是手写列表，拼错字段时应该报错而不是静默丢掉。版本前向兼容靠 `schema_version` 门槛保证（§4.2） |
| 16 | 设置页新建 `settings/dock_page.rs` | `settings.rs` 是一个文件加 `SettingsSection` 枚举和 `ALL` 数组；`settings/` 目录下只有 `tests.rs` | 新增一个枚举值，内容放进新文件 `settings/quick_launch.rs`，从 `settings.rs` 声明（§13） |
| 17 | 没提进程生命周期（v2 初稿以“不常驻”为前提，Q5 确认后已由 §15.0 取代） | 主窗口在托盘里时进程存活；托盘关闭并关掉窗口后进程会退出。`launcher_only` 常驻模式只服务 Launcher | 明确：快速启动只在 LumaFiles 运行时可用，`launcher_only` 模式不在本期范围（§15） |
| 19 | （Q5 确认后补充）只要快速启动窗口存在，关闭主窗口就不会退出进程 | gpui Windows 后端只在最后一个窗口关闭时退出（`platform.rs:718-720`）；IPC 挂在主窗口上 | 常驻时隐藏主窗口而不是销毁；不常驻时显式调用 `cx.quit()`（§15.0） |
| 18 | （Q1 确认后补充）常驻模式直接打开 `state.redb` | redb 是单进程独占锁；常驻模式下没有 RootView 和托盘，也不开 IPC 服务端 | 常驻模式不打开 KV，文件夹交给主程序打开，热键重复时静默处理（§15.1） |

---

## 1. 执行总则

1. **只做当前里程碑**（§17）。一个里程碑对应一个分支或 PR，通过验收后再开始下一个。
2. 现有 Launcher、Explorer、托盘和配置文件的行为与格式**保持不变**。`config.toml`、`appearance.toml`、`search.toml` 不新增字段。
3. **不新增 crate**。新依赖只能从 §3.3 的白名单里选。
4. `.rules` 里的硬规则（这里摘要列出，原文优先）：
   - 不用 `unwrap()`、`expect()`，用 `?` 传播错误；索引访问要防越界。
   - 禁止 `let _ = fallible()`，改用 `.log_err()` 或显式处理。
   - 注释只写“为什么”，不写复述代码的注释。公共项必须有文档注释，因为工作区开了 `missing_docs`，CI 用 `-D warnings`。
   - 禁止 `mod.rs`，写成 `foo.rs` 加 `foo/` 子目录。
   - 变量名用完整单词。异步闭包里的 clone 用变量遮蔽来限定作用域。
   - **不做规格之外的“创意添加”**。
5. 每个 PR 必须在 Windows 上通过：
   ```powershell
   cargo fmt --all -- --check
   cargo clippy --release --workspace --all-targets -- -D warnings
   cargo test --release -p nohrs-core -p nohrs-services -p nohrs-launcher -p nohrs-pages --quiet -- --test-threads=1
   cargo build --release -p nohrs
   ```
   另外，非 Windows 平台的编译不能被破坏，CI 的 Linux 任务会编译 `default-members`。
6. PR 描述必须包含：改动的文件、新增的公共项、测试命令和结果、§16 的性能数字、§19 的未决问题。不允许把失败的测试改成 `#[ignore]` 来绕过。

---

## 2. 命名

| 用途 | 名称 |
|---|---|
| 界面文案（葡萄牙语键） | `t!("Inicialização rápida")`，中文翻译为“快速启动” |
| Rust 模块 | `quick_launch` |
| 配置文件 | `quicklaunch.toml`，放在配置目录根部 |
| KV 命名空间 | `quick_launch` |
| gpui action | `ToggleQuickLaunch` |
| 日志 target | `quick_launch` |

**禁止**在新代码里使用 `dock` 一词，避免和现有 `dock.rs`、`DockSide` 混淆。

---

## 3. 模块划分与文件归属

### 3.1 新增文件（只允许新增这些）

| 文件 | 内容 | 平台 |
|---|---|---|
| `nohrs-core/src/config/quick_launch.rs` | 数据模型、解析、校验、迁移、序列化（纯逻辑，不做 I/O） | 全平台 |
| `nohrs-services/src/launch.rs` | `LaunchAction` trait、注册表、路径变量展开、URL 校验、错误类型；非 Windows 平台的桩实现 | 全平台 |
| `nohrs-services/src/launch/windows.rs` | 内置类型的 Win32 实现、“已在运行”检测 | `#[cfg(windows)]` |
| `nohrs-pages/src/quick_launch.rs` | `QuickLaunchState`（唯一的内存状态源）、加载/保存/热重载、窗口创建与显示隐藏、图标缓存 | Windows 为主，其他平台能编译即可 |
| `nohrs-pages/src/quick_launch/view.rs` | 分组栏、网格视图、列表视图、键盘与鼠标交互、右键菜单、拖放 | 同上 |
| `nohrs-pages/src/settings/quick_launch.rs`（M3） | 设置页 | 同上 |
| `nohrs-cli/src/quick_launch.rs`（M3） | `noh quick-launch` 子命令 | 全平台 |

### 3.2 允许修改的现有文件（只做列出的事）

| 文件 | 允许的改动 |
|---|---|
| `nohrs-core/src/config/paths.rs` | 新增 `quick_launch_file()`（= `active_config_dir().join("quicklaunch.toml")`）和 `quick_launch_icon_dir()`（= `cache_dir().join("quick_launch_icons")`），并为它们写测试。**不改** `config_file_for` |
| `nohrs-core/src/config.rs` | `pub mod quick_launch;` |
| `nohrs-services/src/nohrs_services.rs` | `pub mod launch;` |
| `nohrs-services/Cargo.toml` | 只在 `[target.'cfg(windows)'.dependencies]` 下的 `windows` 里**追加 feature**，版本保持 `0.61` |
| `nohrs-launcher/src/hotkey.rs` | 按 §10 重构为多热键。`default_chord()`、`describe()` 和现有测试的语义都不变 |
| `nohrs-launcher/src/nohrs_launcher.rs` | 导出 §10 的新 API |
| `nohrs-pages/src/nohrs_pages.rs` | `pub mod quick_launch;`，导出 `ToggleQuickLaunch` |
| `nohrs-pages/src/tray.rs` | 菜单加一项 `MENU_QUICK_LAUNCH = 3`，新增 `take_quick_launch()` 标志，做法照抄 `SHOW`/`take_show()`；新增 `pub(crate) fn request_show()`、`request_quit()`，只设置现有标志（§15.0）；把 `mod platform` 改为 `pub(crate)`（§11） |
| `nohrs-pages/src/root.rs` | 在现有的发布循环里读取 `take_quick_launch()` 并 dispatch `ToggleQuickLaunch`；注入“在新标签打开文件夹”的回调；`on_window_should_close` 改为按 `close_action` 分发（§15.0） |
| `nohrs-pages/src/settings.rs`（M3） | `SettingsSection` 增加 `QuickLaunch`，同步 `ALL` 数组的长度和顺序（放在 `Integrations` 之后） |
| `nohrs/src/app.rs` | 初始化快速启动；通过 §10 的 API 注册它的全局热键；注册 `ToggleQuickLaunch` 的 action 处理 |
| `nohrs-core/src/i18n/strings.rs`、`extra.rs` | 新文案的翻译行（§14） |
| `docs/config.md`、`docs/config.schema.json`（M3） | 补充 `quicklaunch.toml` 的说明 |

### 3.3 依赖白名单
- 工作区已有的依赖都可以直接用：serde、toml 0.9、notify、tracing、anyhow、thiserror、gpui、gpui-component、global-hotkey 0.8、nucleo-matcher、`nohrs-store`、tempfile。
- 允许给 `windows = "0.61"` 加 feature。
- **禁止**：tokio（见 ADR 0004）、新的 GUI 或图片库、`toml_edit`（如果要保留注释，先提出来）、新的哈希库（用 `std::hash::DefaultHasher`；不同版本之间不稳定也没关系，最多是缓存失效）。

### 3.4 依赖方向
`core` 不依赖任何其他 crate；`services` 不依赖 gpui（它的 gui feature 不用于本功能）；`pages` 可以用 core、services、store、ui；`launcher` 不依赖 `pages`；`app` 负责把它们接起来。

---

## 4. 数据模型（`nohrs-core/src/config/quick_launch.rs`）

```rust
pub const SCHEMA_VERSION: u32 = 1;
pub const MAX_GROUPS: usize = 64;
pub const MAX_ITEMS_PER_GROUP: usize = 512;
pub const MAX_SLOT: u32 = 4095;
pub const MAX_GROUP_REFERENCES: usize = 16;

#[serde(default, deny_unknown_fields)]
pub struct QuickLaunchConfig { pub schema_version: u32, pub window: WindowSettings,
    #[serde(rename = "group")] pub groups: Vec<Group> }

#[serde(default, deny_unknown_fields)]
pub struct WindowSettings {
    pub hotkey: String,            // 默认 "ctrl+alt+Space"（global-hotkey 的 FromStr 格式，见 §10）；空串 = 不注册
    pub view: View,                // grid | list，默认 grid
    pub columns: u8,               // 2..=16，默认 8
    pub cell_size: u16,            // 64..=160，默认 96
    pub icon_size: u16,            // 只能取 16/24/32/48/64/96，默认 48；其他值取最近的
    pub list_columns: u8,          // 1..=3，默认 2
    pub auto_hide: bool,           // 默认 true
    pub topmost: bool,             // 默认 false
    pub close_after_launch: bool,  // 默认 true
    pub on_running: OnRunning,     // activate | new，默认 activate（ask 推迟到 M4）
    pub keep_running: bool,        // 默认 true（Q5）：关闭主窗口后进程保持运行，快速启动仍可用，见 §15.0
}

#[serde(deny_unknown_fields)]
pub struct Group { pub id: String, pub name: String,
    #[serde(default, rename = "item")] pub items: Vec<Item> }

#[serde(deny_unknown_fields)]
pub struct Item {
    pub slot: u32,
    #[serde(rename = "type")] pub kind: String,  // app|folder|file|url|uwp|command|group|plugin:<id>
    pub name: String,
    #[serde(default)] pub id: Option<String>,    // 只有被 group 类型引用时才需要
    #[serde(default)] pub path: Option<String>,
    #[serde(default)] pub args: Option<String>,
    #[serde(default)] pub cwd: Option<String>,
    #[serde(default)] pub admin: bool,
    #[serde(default)] pub icon: Option<String>,
    #[serde(default)] pub tags: Vec<String>,
    #[serde(default)] pub items: Vec<String>,    // 只用于 kind = "group"
    #[serde(default)] pub extra: toml::Table,    // 只用于 command / plugin:*，其他类型必须为空
}
```

- **v1 里的条目级全局热键 `hotkey` 已删除**：每个热键都要占一个系统热键名额，也会增加冲突。如果确实需要，再另行立项。
- 枚举都用 `#[serde(rename_all = "snake_case")]`。
- 所有公共项都要写文档注释。

### 4.1 标识符规则（由 §6 的 KV 键规则推出）
- `Group.id` 和 `Item.id`：`^[a-z0-9_]{1,32}$`。
- 不符合规则的 id：加载时报 warning，**整个分组或条目跳过**。不要自动改名，自动改名会让 `group` 类型的引用悄悄断掉。
- `name`：去掉首尾空白后长度为 1..=64 个字符。超长就截断并报 warning；为空就跳过并报 warning。

### 4.2 解析与校验（每一行都要有对应测试）

```rust
pub fn parse(text: &str) -> Result<Parsed, QuickLaunchError>;   // Parsed { config, warnings: Vec<Warning> }
pub fn to_toml(config: &QuickLaunchConfig) -> Result<String, QuickLaunchError>;
pub fn migrate(config: QuickLaunchConfig) -> QuickLaunchConfig;  // 目前只有 v1，原样返回
```

| 情况 | 处理 |
|---|---|
| 文件不存在 | 由调用方判断，返回默认配置：一个 `id = "general"` 的空分组，名字取当前语言的 `t!("Geral")`。**不写文件** |
| 语法错误、类型错误、未知字段 | `Err(Parse { line, column, message })`，行列号从 toml 错误的 span 换算 |
| `schema_version` 大于当前版本 | `Err(UnsupportedVersion(found))`；调用方之后**禁止写回** |
| `schema_version` 等于 0 或字段缺失 | 当作 1 处理 |
| 文件以 UTF-8 BOM（U+FEFF）开头 | 去掉 BOM 后正常解析（v2.3；PowerShell 5.1 和旧版记事本会写入 BOM） |
| 数值越界 | 夹到合法范围内，并报 warning |
| 分组 id 重复、同组 slot 重复、条目 id 重复 | 保留第一个，丢弃后面的，并报 warning |
| 超过 `MAX_*` 限制 | 截断，并报 warning |
| 未知的 `kind` | 保留条目，写回时原样输出；界面显示“不支持的类型”，点击不执行任何动作 |
| `group` 引用了不存在的 id、引用了自己、形成环、嵌套超过 8 层、引用超过 16 个 | 去掉有问题的引用，并报 warning |
| 非 command/plugin 类型的条目带了 `extra` | 清空 `extra`，并报 warning |

---

## 5. 读写与热重载（`nohrs-pages/src/quick_launch.rs`）

### 5.1 读取
- 启动时在**后台线程**读文件（`cx.background_spawn`），加 `#[allow(clippy::disallowed_methods)] // runs on the background executor`。
- 窗口在第一次加载完成前打开时，先显示空的分组栏和“正在加载”提示，不阻塞。

### 5.2 写回
- 所有修改先改 `QuickLaunchState`，然后**节流 300 ms**，在后台合并成一次保存。
- 保存步骤：`to_toml` → 写入 `quicklaunch.toml.tmp` → `sync_all` → 把旧文件复制成 `quicklaunch.toml.bak`（只保留一份）→ `std::fs::rename` 覆盖原文件。任何一步失败，原文件保持不动，并在界面上用 toast 提示。
- 如果加载时得到 `UnsupportedVersion` 或 `Parse` 错误，就进入**只读状态**：所有编辑入口置灰，界面顶部显示错误条，**禁止保存**（否则会覆盖用户写坏或更新版本的文件）。
- 写回会丢掉用户手写的注释。设置页的“打开 quicklaunch.toml”按钮旁要提示这一点。
- 便携模式切换时，`storage::copy_missing_files` 会复制配置目录根部的**所有**文件，所以 `.tmp` 和 `.bak` 也会被一起复制。这是可以接受的，不需要改 storage.rs。

### 5.3 热重载
- 新建一个 `ConfigWatcher::new(&quick_launch_file(), …)`。它的回调只往 channel 里发信号，前台循环做重载，写法参照 `root.rs::start_config_watch`，但**复用同一个 400 ms 循环**，不要另开定时器。
- `ConfigWatcher` 也会对 `appearance.toml` 和 `search.toml` 的变化发信号。重载前计算文件内容的 `DefaultHasher` 哈希，和上次加载或保存时的哈希相同就跳过。这一条同时解决了“自己保存后又触发重载”的回环。
- 重载成功：替换状态并刷新界面。重载失败：保留旧配置，进入 §5.2 的只读状态，并显示行号。
- 如果在节流窗口内还有未保存的修改，而外部文件又变了：**以外部文件为准**，丢弃内存中的修改，用 toast 告知用户。

---

## 6. 高频状态（`state.redb`，通过 `nohrs_store::KvStore`）

`app.rs` 里已经有 `open_host_store()`，返回 `Option<Arc<dyn KvStore>>`，把它传给 `QuickLaunchState`。键必须符合 `KvKey` 规则：`<namespace>.<name>`，并且全部是小写 ASCII、数字和 `_`。

| 键 | 值（serde_json 编码） |
|---|---|
| `quick_launch.window_<monitor>` | `{x, y, width, height}`。`<monitor>` 取显示器设备名经 `DefaultHasher` 得到的 16 位十六进制 |
| `quick_launch.last_group` | 分组 id |
| `quick_launch.usage_<group_id>_<slot>` | `{count: u32, last: u64}` |
| `quick_launch.trusted_<hash>`（M4） | `true`，记录 command 条目已经确认过 |

- 用 `KvKey::new("quick_launch", name)` 构造键，不要手拼字符串。
- 没有 store，或者读写出错：功能照常可用，只是不记住位置和使用次数，用 `.log_err()` 记录。
- 删除分组或条目时，同时删除对应的 `usage_*` 键，用 `batch`。

---

## 7. 路径变量展开（`nohrs-services/src/launch.rs`）

```rust
pub fn expand(raw: &str, context: &ExpandContext) -> Result<PathBuf, ExpandError>;
pub struct ExpandContext { pub portable_root: Option<PathBuf>, pub program_dir: PathBuf, pub config_dir: PathBuf }
```
- 支持的变量：`%NAME%`（环境变量，大小写不敏感）、`{portable}`、`{luma}`（程序所在目录）、`{config}`。
- 变量未定义时返回 `Err(Undefined(name))`；非便携模式下用到 `{portable}` 时返回 `Err(NotPortable)`。**禁止**保留未展开的字符串继续去启动。
- 只展开一层，不递归。
- 相对路径按 `{luma}` 解析。
- `ExpandContext` 由调用方从 `nohrs_core::config::paths` 构造，services 自己不读全局状态，方便测试。
- 配置里存用户写的原始字符串。界面拖入时，如果路径在便携根目录下，自动改写成 `{portable}\…`。

---

## 8. 启动服务

### 8.1 接口（`launch.rs`，全平台）

```rust
pub trait LaunchAction: Send + Sync + 'static {
    fn kind(&self) -> &str;
    fn health(&self, item: &Item, context: &ExpandContext) -> Health;  // 只看路径是否存在，不打开文件、不访问网络
    fn launch(&self, item: &Item, request: &LaunchRequest) -> Result<LaunchOutcome, LaunchError>;
}
pub enum Health { Ok, Missing, Invalid(String), Unknown }
pub enum LaunchMode { Normal, Admin, ForceNew }
pub enum LaunchOutcome { Started, Activated, Cancelled, OpenFolder(PathBuf) }
pub struct LaunchRequest { pub mode: LaunchMode, pub on_running: OnRunning, pub expand: ExpandContext }
pub struct LaunchRegistry { … }   // with_builtins() / register() 重复 kind 时返回 Err / get()
```
- `folder` 类型**不自己打开文件夹**，返回 `OpenFolder(path)`，由 pages 层在 LumaFiles 的新标签里打开。这样 services 不需要依赖界面。
- “打开所在位置”也是 pages 层处理：取目标文件的父目录，在新标签打开并选中该文件，**复用 `reveal.rs` 现有的逻辑**。
- 非 Windows 平台：`with_builtins()` 只注册 `folder`、`url`、`file`，后两者用 `open` 或 `xdg-open`；或者全部返回 `Unsupported`。本期选**全部返回 `Unsupported`**（改动最小）。

### 8.2 内置类型的 Windows 实现（`launch/windows.rs`）

| kind | 实现 | 边界 |
|---|---|---|
| `app`（.exe） | `ShellExecuteExW`：`lpFile` 为展开后的路径，`lpParameters = args`，`lpDirectory = cwd`（没有就用 exe 所在目录），`nShow = SW_SHOWNORMAL`；Admin 模式或 `admin = true` 时 `lpVerb = "runas"` | 统一走 `ShellExecuteExW` 而不是 `CreateProcessW`，这样一套代码同时处理 exe、lnk 和 UAC。**禁止**通过 `cmd /c` 拼接命令 |
| `app`（.lnk） | 同上，直接把 lnk 交给 Shell | 不自己解析快捷方式里的参数 |
| `file` | `ShellExecuteExW`，verb 为 NULL | |
| `url` | 先调 `validate_url`（全平台纯函数）：只允许 `http`、`https`、`mailto`；再 `ShellExecuteExW` | `file:`、`javascript:`、`ms-*:`、`vbscript:` 等一律返回 `Blocked` |
| `uwp`（M4） | `IApplicationActivationManager::ActivateApplication` | |
| `command`（M4） | 见 §17 M4 | |
| `group`（M4） | 按顺序启动引用的条目，失败的跳过，最后汇总结果 | |

- UAC 被用户取消（`ERROR_CANCELLED` = 1223）时返回 `Ok(Cancelled)`，**不提示错误**。
- 其他失败返回 `Os { code, message }`，`message` 用 `FormatMessageW` 生成。

### 8.3 “已在运行”就激活窗口（`on_running = activate`，只对 `.exe` 生效）
- 参照 `listary.rs` 和 `file_dialog.rs` 里已有的 Toolhelp 进程枚举写法：`CreateToolhelp32Snapshot` 枚举进程 → `QueryFullProcessImageNameW` → 规范化路径后按**不区分大小写**比较。
- 找到进程后，用 `EnumWindows` 找它的第一个可见、无 owner 的顶层窗口，然后最小化恢复加 `SetForegroundWindow`，写法参照 `foreground.rs`，**不要另写一套**。
- 找不到可见窗口、超过 100 ms、或目标是 `.lnk`：直接启动新实例（.lnk 解析放到 M4 以后）。

### 8.4 线程、COM、unsafe（硬性要求）
- `launch()`、`health()`、图标提取**不能在 gpui 前台线程执行**。
- **v2.2 修订（M1 验收 B3）：两条专用线程，不共用。**由 pages 层持有，通过 channel 接收请求、返回结果。原因是 Shell 调用可能卡在网络路径或休眠的硬盘上，不能占用 gpui 的后台线程池，理由同 `PERFORMANCE.md` §1；同时两类请求不能互相阻塞：
  - `quick-launch`：只处理启动请求。
  - `quick-launch-icons`：只处理图标请求。请求在**队列中**等待超过 2 s 就直接丢弃，不再执行。这条线程卡住时，界面继续显示占位图标，**启动不受任何影响**。
  - v2.1 的“一条线程同时处理启动和图标”作废：首次打开时几十个图标请求会排在点击之前，网络盘上一个卡住的图标也会让所有启动挂起。
  - 验收：用一个会阻塞的假图标提取器卡住图标线程，启动请求仍然要在 100 ms 内得到回复。
- 两条线程启动时都调用 `CoInitializeEx(COINIT_APARTMENTTHREADED)`，图标线程再调用 `thumbnails::init_thread()`。线程结束时调用 `CoUninitialize`，用 RAII guard 包住。
- 每个请求都包在 `std::panic::catch_unwind` 里，panic 转成 `LaunchError::Internal`，线程继续运行。
- 请求带上代次号，过期的结果直接丢弃。
- `unsafe` 只能出现在 `launch/windows.rs` 和 `quick_launch.rs` 的窗口显示/隐藏函数里。每处都写**逐项** `#[allow(unsafe_code)]`，并在前面加 `// SAFETY:` 说明。
- 句柄统一用 `windows` crate 的 RAII 类型或 `OwnedHandle`。

### 8.5 错误类型
```rust
#[derive(Debug, thiserror::Error)]
pub enum LaunchError { NotFound(PathBuf), Expand(#[from] ExpandError), Unsupported(String),
    Blocked(String), Os { code: u32, message: String }, Internal(String) }
```
界面上只在窗口底部显示一行翻译后的文字（v2.3：M1 验收时确认用底部一行代替 gpui-component 的通知层，因为通知层会触发 gpui 的嵌套更新并 abort），详情写进日志，**不弹模态框**。M4 里 command 的首次确认是唯一例外。

---

## 9. 图标

- **复用** `explorer/thumbnails.rs` 里的 `platform::extract(path, size, Mode::…)` 和 `platform::init_thread()`。已经核实：`mod platform`（第 996 行和第 1211 行两个 cfg 分支）和 `enum Mode`（第 977 行）现在都是私有的，**允许且只允许**把这两处改成 `pub(crate)`，**不复制代码**。
- 查找顺序：先用 `item.icon` 自定义图标（.ico、.png、`exe,索引`；带索引的情况交给 M2 用 `PrivateExtractIconsW`，M1 只支持 .ico 和 .png）→ 再用 `extract`（`.lnk` 交给 Shell 解析）→ 最后用按类型区分的内置占位图标。
- 尺寸取 `icon_size × 窗口缩放`，向上取到 {16, 24, 32, 48, 64, 96, 128, 256} 中的一档。
- 磁盘缓存放在 `quick_launch_icon_dir()/<hash>.png`，`hash = DefaultHasher(规范化路径, 修改时间, 文件大小, 尺寸)`。PNG 编码用 pages 已经依赖的 `image` crate。
- 内存缓存用 LRU，最多 512 项。磁盘缓存总量上限 64 MB，超出后在启动后的空闲时间按最早修改时间删除。
- 显示流程：先用缓存，缓存没有就显示占位图标，同时异步提取。单次提取超时 2 s；超时的路径在本次会话内不再重试。

---

## 10. 全局热键（重构 `nohrs-launcher/src/hotkey.rs`）

目标：**一个 `GlobalHotKeyManager`、一个 40 ms 轮询循环，按 id 分发到多个回调。**

```rust
pub struct HotkeyId(u32);
/// 第一次调用时创建 manager 和轮询循环，之后复用。
pub fn register(cx: &mut App, hotkey: HotKey, on_press: impl Fn(&mut App) + 'static) -> Result<HotkeyId>;
pub fn unregister(cx: &mut App, id: HotkeyId) -> Result<()>;
pub fn parse(text: &str) -> Result<HotKey>;   // 用 global_hotkey 的 FromStr；如果 0.8 版没有实现，在 PR 里记成未决问题，并自己实现 ctrl/alt/shift/win + 键名的解析
```
- `Registration` Global 改为持有 `manager` 和 `HashMap<u32, Rc<dyn Fn(&mut App)>>`。`fold_presses` 改为对每个 id 分别维护 `held` 状态，用 `HashMap<u32, bool>`。
- 现有的 `install(cx, on_summon, …)` **签名不变**，内部改为调用 `register`。Linux 的 portal 路径**不动**。
- 现有测试**全部保留**。新增测试：两个 id 互不干扰；按住其中一个不会触发另一个；`unregister` 之后不再触发。
- 快速启动的热键：`app.rs` 读取 `quicklaunch.toml` 的 `window.hotkey`，解析成功就 `register`，回调里 dispatch `ToggleQuickLaunch`。注册失败（比如热键被占用）时：写日志，设置页显示“热键被占用”，其他功能照常。
- 配置里的 `hotkey` 被修改后，先 `unregister` 再 `register`。如果新热键注册失败，就保留旧热键并提示用户。
- 默认值 `ctrl+alt+Space`。**不能**和 Launcher 的 `Ctrl+Shift+Space` 冲突；如果用户配置成相同的组合键，拒绝注册并提示。

---

## 11. 窗口（`quick_launch.rs`）

- **常驻**：第一次召唤时用 `cx.open_window` 创建（`WindowKind::PopUp`，带自定义标题栏，参照 `launcher_window_options` 的写法），之后只显示或隐藏，不销毁。保存它的 `WindowHandle`。
- **隐藏和显示**：gpui 在 Windows 上没有窗口级的隐藏。用 `crate::ipc::window_hwnd(window)` 拿到 HWND，再调用 `tray.rs` 的 `platform::show_window(hwnd)` 和 `platform::hide_window(hwnd)`。已经核实：这两个函数只调用 `ShowWindow`、`SetForegroundWindow`，**不修改** `HIDDEN`，非 Windows 平台也有空实现。允许把 `tray.rs` 里两个 cfg 分支的 `mod platform` 改为 `pub(crate)` 后复用。**禁止**调用 `tray::hide(window)` 或 `tray::show(window)`，因为它们会执行 `set_hidden(…)`，改变主窗口的托盘状态。
- 如果窗口句柄失效（比如被系统关闭），下一次召唤时重新创建。
- **失去焦点自动隐藏**：用 `cx.observe_window_activation(window, …)`（定义在 `third_party/gpui/src/app/context.rs`），在回调里通过 `window.is_window_active()` 判断状态。仅当 `auto_hide` 为真且没有置顶时生效，失焦 150 ms 后隐藏；这个延时用 `cx.background_executor().timer` 实现，是一次性的，不算新增轮询。拖放进行中、右键菜单打开、对话框打开时不隐藏。
- 默认尺寸：网格 980×875，列表 660×860（逻辑像素）。最小 360×300，可以调整大小。
- 位置按显示器记住（§6）。恢复时如果窗口完全不在任何显示器内，就居中显示在鼠标所在的显示器上。
- **置顶**：gpui 没有提供运行时切换置顶的接口时，用 `SetWindowPos(HWND_TOPMOST / HWND_NOTOPMOST)`，同样只写在 `quick_launch.rs` 里。
- **召唤方式**：全局热键（切换显示）；托盘菜单项；`ToggleQuickLaunch` action（M3 在设置页和工具栏放入口，M1 不做工具栏按钮）。
- 颜色、字体、圆角都从 `nohrs_ui::theme::theme` 读取，**禁止写死颜色**。

---

## 12. 交互

### 12.1 布局
- 左侧分组栏宽 120 px，选中项用 `theme::accent()` 画下划线。
- 网格：每行 `columns` 格，行数为 `ceil((最大 slot + 1) / columns)`，再加一行空槽，方便往后添加。
- 列表：`list_columns` 列。
- 空槽画成空底板，可以作为拖放目标。
- 格子数量超过可视区域时**必须**用 `uniform_list` 做虚拟化。名字单行截断，悬停显示完整名称。
- 失效（`Missing`）的条目显示为灰色并加角标。健康检查在窗口第一次显示后的空闲时间进行，每批 32 个，在专用线程上执行。

### 12.2 操作（按里程碑划分）

| 操作 | 行为 | 里程碑 |
|---|---|---|
| 左键单击 / `Enter` | 启动；`close_after_launch` 为真时隐藏窗口 | M1 |
| 单击分组栏 / `Alt+1..9` | 切换分组 | M1 |
| `Esc` | 有过滤内容时先清空过滤，否则隐藏窗口 | M1 |
| 方向键 | 移动选中项，跳过空槽，在边界处停住（不循环） | M1 |
| 直接打字 | 用 `nucleo-matcher` 在当前分组里按 name 和 tags 过滤 | M2 |
| `Ctrl+单击` / `Ctrl+Enter` | 以管理员身份运行 | M2 |
| `Shift+单击` | 在 LumaFiles 里打开所在位置 | M2 |
| 右键条目 | 以管理员身份运行 / 打开所在位置 / 编辑 / 复制 / 移到分组 ▸ / 删除 | M2 |
| 右键空槽或分组栏 | 新建条目 / 新建分组、重命名、删除（删除非空分组需要确认） | M2 |
| 拖入外部文件 | 用 gpui 的 `ExternalPaths`（参照 `explorer/drag.rs` 和 `view/sidebar.rs`）。放在哪个槽就占哪个槽；如果拖入多个文件，按顺序占后面的空槽 | M2 |
| 组内拖动 | 移到目标槽；目标槽有条目时两者**交换** | M2 |
| 拖到分组栏 | 移到该分组的第一个空槽；没有空槽就追加到末尾 | M2 |

- 从 LumaFiles 自己的文件列表拖进快速启动窗口要走 OLE 跨窗口路径，`third_party/gpui/NOHRS.md` 记录过这里曾经崩溃并已修复。M2 的验收必须**手动测试这个场景**。
- `.lnk` 拖进来时保存 lnk 本身的路径。条目名默认取文件名（去掉扩展名）。

---

### 12.3 M2 的添加与编辑（v2.4 补充）

v2.3 之前只在右键菜单里写了“新建、编辑”，没有规定界面。M2 按下面的约定实现，**不要另行设计**：

| 入口 | 行为 |
|---|---|
| 右键空槽 → 新建条目 | 打开系统文件选择框（gpui `prompt_for_paths`，允许选文件或文件夹），选中后写入这个槽：`.exe`/`.lnk` 记为 `app`，文件夹记为 `folder`，其他文件记为 `file`；`name` 取文件名去掉扩展名 |
| 右键条目 → 编辑 | 弹出一个小对话框，只有 4 个字段：名称、路径（右侧带“浏览…”）、参数、以管理员运行。确定后保存，取消则不改。其余字段（图标、标签、工作目录）留给 M3 的设置页 |
| 右键分组栏 → 新建分组 | id 自动取 `group_<n>`（n 从 1 起，取第一个没被占用的），名称取 `t!("Novo grupo")`，建好后立即进入改名状态 |
| 右键分组栏 → 重命名 | 就地编辑；名称为空就取消 |
| 右键分组栏 → 删除 | 空分组直接删除；非空分组先确认。最后一个分组不能删（菜单项置灰） |

共同约束：
- 只读状态（§5.2）下，以上所有入口和拖放**一律置灰**。
- 每次修改都走 `mark_dirty()` + `touch()`，由 300 ms 节流统一在后台保存，**不要**在事件处理里直接写盘。
- 删除条目或分组时，按 §6 用一次 `batch` 一起删掉对应的 `usage_*` 键。
- 对话框和菜单回调里**不要**在当前窗口的更新期间再去更新同一个窗口（M1 已经因此 abort 过一次）：需要时用 `cx.defer` 或 `window.defer`。
- 新增文案一律走 `t!`，9 种语言都要补齐。

## 13. 设置页（M3）
- `SettingsSection::QuickLaunch` 排在 `Integrations` 之后，内容写在 `settings/quick_launch.rs`，风格参照同文件中的其他 section。
- 页面内容：热键（录制控件，显示是否注册成功）；视图参数；分组列表（增加、删除、改名、拖动排序）；条目编辑表单（类型、名称、路径带“浏览…”、参数、工作目录、管理员、图标、标签）；“打开 quicklaunch.toml”按钮及会丢失注释的提示；只读状态下显示的错误。
- 设置页和快速启动窗口**共享同一个 `QuickLaunchState`**，作为 gpui Global 或 Entity。禁止各自持有一份副本。

---

## 14. i18n
- 新文案一律写成 `t!("葡萄牙语原文")`，并在 `strings.rs` 里补 En/Es、在 `extra.rs` 里补 Fr/De/It/Ru/Ja/Zh 的行。缺任何一种语言，现有的扫描测试都会失败。
- 用户自己填的分组名、条目名不翻译。
- 默认分组名用 `t!("Geral")`，**只在第一次生成默认配置时**取当前语言的值写进内存，之后就是普通文本。
- 托盘菜单项用 `t!("Inicialização rápida")`。

---

## 15. 进程生命周期
### 15.0 主程序常驻（Q5，M1 实现）

BoBO 于 2026-11-22 确认 Q5：主窗口关闭后，即使托盘图标也关着，进程仍要继续运行，让快速启动可以随时召唤。

**已核实的现状：**
- `root.rs:161` 的 `on_window_should_close` 在 `close_to_tray = false`（默认值）时返回 `true`，主窗口被销毁；为 `true` 时调用 `tray::hide(window)` 并返回 `false`。
- gpui 的 Windows 后端在**最后一个窗口关闭时**才调用 `PostQuitMessage`（`third_party/gpui/src/platform/windows/platform.rs:718-720`）。所以快速启动的窗口常驻以后，即使关闭了主窗口，进程也**不会退出**。如果不处理，这会成为一个回归：用户关掉窗口，进程却在后台残留。
- `ipc::serve` 挂在主窗口上。主窗口销毁后 IPC 随之停止，这时再运行 `LumaFiles.exe` 会启动第二个完整实例，和第一个实例争抢 `state.redb` 和热键。

**规定：**
1. 常驻生效的条件：`resident = window.keep_running && 本进程成功注册了快速启动热键`。热键没有注册成功时，常驻没有意义，按不常驻处理。
2. 关闭主窗口时的行为写成一个纯函数，放在 `quick_launch.rs`，并写单元测试：
   ```rust
   pub(crate) enum CloseAction { Close, HideToTray, Quit }
   pub(crate) fn close_action(close_to_tray: bool, resident: bool) -> CloseAction
   ```
   | `close_to_tray` | `resident` | 结果 | 说明 |
   |---|---|---|---|
   | true | 任意 | `HideToTray` | 现有行为不变 |
   | false | true | `HideToTray` | **新增**：主窗口隐藏而不是销毁，RootView、IPC、redb 都保留，之后再运行 `LumaFiles.exe` 会立即唤出这个窗口 |
   | false | false | `Quit` | 先把快速启动没保存的修改写入磁盘，再调用 `cx.quit()`。不能只返回 `true`，否则隐藏的快速启动窗口会让进程残留 |
3. `root.rs` 的 `on_window_should_close` 只按 `close_action` 的结果分发。`HideToTray` 复用现有的 `tray::hide(window)`，**不另写隐藏逻辑**。
4. 托盘图标继续遵循现有的 `tray_icon` 设置：`always` 一直显示，`background` 在主窗口隐藏后出现，`never` 不显示。**不修改托盘逻辑。**
5. **退出和唤回入口**：`tray_icon = never` 并且常驻时，进程没有任何可见的界面，所以快速启动窗口的标题栏必须提供一个“⋯”菜单（M1），包含：
   - `t!("Mostrar janela")`（复用托盘现有文案）：唤回主窗口
   - `t!("Sair do LumaFiles")`（新文案）：退出整个进程

   做法是在 `tray.rs` 里新增 `pub(crate) fn request_show()` 和 `pub(crate) fn request_quit()`，它们只设置现有的 `SHOW`、`QUIT` 原子标志，由 `root.rs` 现有的循环处理。**不新增循环。**
6. 开机即可用：复用现有的 `--background` 启动参数和“随 Windows 启动”设置，**不新增开机自启机制**。
7. 设置页（M3）在“快速启动”页提供 `keep_running` 开关，文案为“关闭主窗口后仍保持快速启动可用”。M3 之前只能在 toml 里修改。
8. 和 §15.1 的关系：主程序常驻以后，一般不再需要 `launcher_only` 进程。两者同时运行时，按 §15.1 的“热键重复”规则处理。

**验收（加进 §16.3）：**
- `close_to_tray`（2 种）× `keep_running`（2 种）× `tray_icon`（3 种），共 12 种组合，逐一测试关闭主窗口后：进程是否还在、热键是否可用、托盘图标状态、再运行 `LumaFiles.exe` 能否唤回原窗口（不能出现第二个进程）。
- `keep_running = false` 时，关闭主窗口后进程在 1 秒内退出，任务管理器里没有残留。
- 常驻状态下隐藏 8 小时：工作集内存增长不超过 10%，句柄数没有持续增长。
- 用“⋯ → 退出”退出后，`quicklaunch.toml` 里的修改没有丢失。

### 15.x 其他
- **`launcher_only` 常驻模式（`LumaFiles launcher`）也要提供快速启动**（BoBO 于 2026-11-22 确认 Q1，放在 M2 实现）。
- 退出时先注销热键、停止专用线程（给它发退出消息，最多等 500 ms），再把还没保存的修改**同步**写一次，作为退出路径上的例外，需加 allow 并注明原因。

### 15.1 常驻模式的约束（M2）
已核实 `app.rs`：常驻模式在 `open_keep_alive_window` 之后直接 `return`，没有 `RootView`、托盘和 IPC 服务端，也**不调用** `open_host_store()`。据此规定：

| 方面 | 规定 | 原因 |
|---|---|---|
| 初始化 | 在 `launcher_only` 分支中、`return` 之前，用与主模式**同一个**初始化函数注册快速启动和它的热键 | 两种模式只维护一份代码 |
| `state.redb` | 常驻模式**禁止打开**，`QuickLaunchState` 以“无 KV”方式运行：不记窗口位置和使用次数，窗口按 §11 规则居中显示 | redb 是单进程独占锁。常驻进程先拿到锁，主程序就会失去标签和会话恢复 |
| 打开文件夹 / 打开所在位置 | 先调用 `nohrs_pages::ipc::request_open(path)`，交给正在运行的主程序处理；返回 `false`（主程序没运行）时，启动 `std::env::current_exe()` 并把路径作为参数传入，新实例自己会打开它 | 常驻模式里没有 Explorer 窗口 |
| 托盘菜单项 | 常驻模式没有托盘，不提供这个入口 | |
| 热键重复 | 主程序和常驻进程同时运行时，两边都会尝试注册 `Ctrl+Alt+Space`，后注册的会失败。后注册的一方**只写 `info` 日志**，不显示“热键被占用”的提示；按下热键时由先注册的一方响应 | 同一用户同时运行两个进程属于正常情况 |
| 配置文件 | 两个进程可能同时编辑 `quicklaunch.toml`。依靠 §5.2 的原子写和 §5.3 的哈希比较热重载，**后保存的覆盖先保存的**，不做合并 | 少见情况，可以接受 |
| 判断“已被自己占用” | 注册失败时用 `ipc::is_running()` 区分：主程序在运行，按上面一行处理；不在运行，按 §10 的“热键被占用”提示 | |

验收（加进 §16.3）：只开常驻模式时，快速启动的热键、启动程序和打开文件夹都正常（文件夹会拉起主程序）；常驻模式和主程序同时运行时，主程序的会话恢复不受影响，热键只触发一个窗口。

---

## 16. 测试与验收

### 16.1 自动测试（必须有）
- `config/quick_launch.rs`：§4.2 表里每一行至少一个测试；`parse → to_toml → parse` 往返结果相等；id 规则；越界夹值；group 引用成环。
- `launch.rs`：`expand` 的每种变量，以及未定义、非便携模式、相对路径的情况；`validate_url` 的白名单和黑名单；注册表重复注册返回错误。
- `hotkey.rs`：§10 新增的三个测试，加上原有测试全部通过。
- `paths.rs`：两个新函数在便携和非便携两种配置档下的路径。
- `quick_launch.rs` 的纯逻辑部分（不打开窗口）：槽位交换、拖到分组栏时找空槽、方向键跳过空槽和边界、节流合并、哈希跳过重载、只读状态禁止保存、KV 键构造。
- 原子写：模拟写入失败（目标是只读目录），原文件内容不变。
- 真正调用 Win32 的测试标记 `#[ignore]`，在 PR 里贴出手动运行的结果：启动 `notepad.exe`、带参数启动、UAC 选“否”、激活已经在运行的记事本。

### 16.2 性能（release 构建，PR 里贴中位数）

| 指标 | 门槛 |
|---|---|
| 窗口已创建：从收到召唤回调到第一帧 | ≤ 50 ms（热键轮询本身另有最多 40 ms 的延迟，不计入） |
| 第一次召唤（创建窗口并读取图标缓存） | ≤ 300 ms |
| 200 个条目的分组之间切换 | ≤ 16 ms |
| `parse` 500 个条目 | ≤ 5 ms |
| 前台线程上任何一次快速启动相关的调用 | ≤ 8 ms，用 `tracing` 的 span 测量 |
| 新增定时器或轮询 | **0 个**（复用热键的 40 ms 循环和配置重载的 400 ms 循环） |

### 16.3 稳定性（手动测试，结果写进 PR）
- 条目指向已删除的 exe、断开的网络路径、休眠的移动硬盘：界面不卡顿，点击后只出现 toast。
- `quicklaunch.toml` 被写成乱码、清空、设为只读、`schema_version = 99`：程序不崩溃，表现符合 §4.2 和 §5.2，用户的文件没有被覆盖。
- 连续快速按热键 50 次：窗口不重复出现，程序不崩溃。
- Launcher 热键 `Ctrl+Shift+Space` 和快速启动热键同时可用，互不影响。
- 主窗口隐藏在托盘时召唤快速启动，再关闭它：主窗口仍在托盘里，托盘图标状态不变。
- 便携模式拷贝到另一台机器：配置和图标缓存的路径正确，`{portable}` 条目可以启动。

---

## 17. 里程碑

| 里程碑 | 只做这些 | 不做 |
|---|---|---|
| **M1** | §4 全部；§5；§6；§7；§8 中 `app`、`folder`、`url` 三种类型及 8.3、8.4、8.5；§9（不支持 exe,索引）；§10；§11；§12.2 标为 M1 的行；§14；托盘菜单项；**§15.0 主程序常驻，包括标题栏“⋯”菜单** | 拖放、右键菜单、过滤、设置页、CLI |
| **M2** | §12.2 标为 M2 的行；`file` 类型；`exe,索引` 格式的图标；健康检查；使用次数统计；**§15.1 常驻模式支持** | 设置页 |
| **M3** | §13 设置页；`noh quick-launch list/add/remove/run/check`；Launcher 数据源：`ranking::ItemKind` 新增 `QuickLaunch` 变体，输入 `@` 前缀时只搜快速启动条目，不改现有权重；`docs/config.md` 和 schema | 插件 |
| **M4** | `uwp`；`group`；`command`（`extra.shell` 只能是 `cmd`、`pwsh`、`powershell`、`python`；`extra.script` 作为参数传入；`extra.hidden` 对应 `CREATE_NO_WINDOW`；**第一次运行必须弹窗确认**，确认记录用脚本内容的哈希存进 KV，脚本一改就重新确认）；`on_running = ask`；只读的动态分组（`source = "folder:…" / "recent" / "most_used"`） | 插件宿主、云同步、边缘停靠、从第三方导入（另行立项） |

每个里程碑结束时，在 `docs/launcher.md` 里补充或更新“Quick Launch”一节，记录实际实现和本文的差异。

---

## 18. 禁止事项
- 不实现插件宿主，只保留 `LaunchRegistry::register`。
- 不联网：不下载图标，不做同步。
- 不修改 `third_party/gpui`；确实需要时先提出来。
- 前台线程上不做文件系统、注册表、进程或 COM 调用。
- `app` 类型不经过任何 shell 启动；只有 `command` 类型允许，并且必须先确认。
- 不写死界面文案和颜色。
- 不新增全局可变 `static`（托盘那种 `AtomicBool` 标志按现有模式新增一个除外），共享状态用 gpui Global 或 Entity。
- 不读写第三方程序（Listary、PowerToys 等）的配置。
- 不改动 Launcher、Explorer、托盘的现有行为；§3.2 中允许的改动除外。

---

## 19. 未决问题

### 19.1 执行方遇到规格没写到的情况
1. 选**改动最小、行为最保守**的做法；
2. 在 PR 描述的“未决问题”一节写清楚：位置、遇到的情况、选择了什么做法、还有哪些备选；
3. 不为此扩大改动范围。代码注释照 `.rules` 的要求，只写“为什么这样做”。

### 19.2 需要产品负责人（BoBO）拍板的问题
以下问题先按括号里的默认做法实现，等你确认后再改：

| # | 问题 | 默认做法 |
|---|---|---|
| Q1 | 快速启动是否也要在 `launcher_only` 常驻模式下可用 | ✅ **已确认：需要**（2026-11-22），放在 M2，约束见 §15.1 |
| Q2 | 默认热键 `Ctrl+Alt+Space` 是否合适（部分输入法和工具会占用） | ✅ **已确认：使用 `Ctrl+Alt+Space`**，可以修改 |
| Q3 | 界面里修改后写回 toml 会丢失手写注释，是否可以接受；还是引入 `toml_edit` 保留注释 | ✅ **已确认：可以接受丢失**，界面上要提示，不引入 `toml_edit` |
| Q4 | 是否需要条目级的全局热键 | ✅ **已确认：不做** |
| Q5 | 主窗口关闭且托盘关闭时，进程是否要为快速启动继续常驻 | ✅ **已确认：继续常驻**（2026-11-22），默认 `keep_running = true`，规则见 §15.0，在 M1 实现 |

5 个问题都已确认，执行方不必再等待产品决定。

---

## 附：给 DeepSeek 的启动提示词

> 你要在 `D:\BoBO\prime-files`（Rust workspace，Windows 版 LumaFiles）里实现“快速启动”（Quick Launch）功能。
> 先完整阅读：仓库根目录的 `.rules`、`clippy.toml`、`docs/config.md`、`docs/launcher.md`、`docs/persistence.md`、`docs/PERFORMANCE.md`；然后阅读 `docs/QuickDock-技术实现方案与边界约定.md`（v2，以它为准）。动手之前，先读规格里点名要复用的文件：`nohrs-launcher/src/hotkey.rs`、`window.rs`，`nohrs-pages/src/tray.rs`、`root.rs`、`foreground.rs`、`explorer/thumbnails.rs`、`explorer/drag.rs`，`nohrs-core/src/config/watcher.rs`、`paths.rs`、`i18n.rs`，`nohrs-store/src/nohrs_store.rs`。
> **只实现里程碑 M1**，严格遵守 §1 执行总则、§2 命名（禁止使用 `dock`）、§3 文件归属和 §18 禁止事项。
> 完成后在 Windows 上运行 §1 第 5 条的全部命令并贴出输出，贴出 §16.2 的性能数字和 §16.3 的手动测试结果，按 §1 第 6 条写 PR 描述，并列出所有未决问题。
