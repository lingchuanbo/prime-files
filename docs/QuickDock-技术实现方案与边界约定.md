# 快速启动（Quick Launch）技术实现方案与边界约定 · v2.9

> v2.1（2026-11-22）：写入 Q1–Q5 的确认结果，新增 §15.0（主程序常驻）和 §15.1（常驻模式）。
> v2.2（2026-11-22，M1 验收后）：§8.4 改为启动、图标两条线程。M1 的具体问题见 `docs/QuickLaunch-M1-验收报告.md`。
> v2.3（2026-11-22，M1 复查后）：§8.5 失败提示改为窗口底部一行；§4.2 补充 BOM 的处理。
> v2.4（2026-11-22，M2 开工前）：新增 §12.3，规定 M2 的添加和编辑界面。
> v2.5（2026-11-22）：新增 §11.1 锁定按钮。
> v2.6（2026-11-23）：新增 §17.1，M3 实现细则。
> v2.7（2026-11-23，M3 验收后）：更正 §17.1 B 中 `gpui_keys_to_hotkey` 的位置，改放到 `nohrs-core`。
> v2.8（2026-11-23，M4 开工前）：新增 §17.2，M4 实现细则。
> v2.9（2026-11-24，M4 补审后）：§12.1 网格和列表的列数改为按窗口大小自动计算（`columns` / `list_columns` 只为旧文件兼容保留，不再使用）；§12.2 打开窗口和切换分组时不再自动选中条目，只有过滤框打字时才选中第一个匹配项。

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

### 11.1 锁定按钮（v2.5，BoBO 2026-11-22 提出）

问题：从资源管理器或桌面往快速启动里拖文件时，要先点击别的窗口去选文件，快速启动窗口一失去焦点就会在 150 ms 后自动隐藏，导致无法拖入。

规定：
- 标题栏右侧、“⋯”按钮**左边**加一个“锁定”按钮（图钉或锁图标）。图标优先用 gpui-component 自带的 `IconName`；没有合适的图标时，把一个 SVG 放进 `nohrs-ui` 的 assets，**不要**用文字或 emoji 代替。
- 锁定状态**复用已有的 `window.topmost` 字段**，不新增配置项：
  - 锁定：`topmost = true`，窗口保持在最前（`set_topmost(window, true)`），失去焦点**不自动隐藏**（现有的 `activation_changed` 已经在 `topmost` 为真时跳过隐藏）。
  - 解锁：`topmost = false`，恢复原来的行为。
- 点击后立即生效，并通过 `mark_dirty()` + `touch()` 保存，下次打开时保持上次的状态。只读状态下按钮仍然可以切换，但只在本次会话内有效、不写回文件，提示文字后面加上“（不会保存）”。
- 按钮外观：锁定时用 `theme::accent()` 高亮显示，解锁时用普通的幽灵按钮样式。提示文字：
  - 解锁时：`t!("Fixar: a janela não se esconde e fica na frente")`，中文为“锁定：窗口不会自动隐藏，并保持在最前”；
  - 锁定时：`t!("Desafixar")`，中文为“解除锁定”。
  - 9 种语言都要补齐。
- 锁定期间的隐藏规则：
  | 触发 | 解锁时 | 锁定时 |
  |---|---|---|
  | 失去焦点 | 150 ms 后隐藏 | **不隐藏** |
  | 启动条目后（`close_after_launch`） | 隐藏 | **不隐藏** |
  | Shift+单击“打开所在位置”后 | 隐藏 | **不隐藏** |
  | Esc（没有过滤内容时） | 隐藏 | 隐藏（用户明确要求） |
  | 再按一次全局热键、托盘菜单 | 切换显示 | 切换显示 |
- 热重载：`quicklaunch.toml` 中的 `topmost` 被外部修改后，对已经打开的窗口也要调用 `set_topmost`，按钮状态同步更新（目前只在创建窗口时设置一次）。
- 测试：切换会改变 `config.window.topmost` 并标记为待保存；只读状态下切换不会保存；锁定时 `close_after_launch` 和 reveal 都不会隐藏窗口；热重载改变 `topmost` 后，状态同步更新。
- 手测：锁定后点击桌面，再从桌面拖一个文件进来，窗口始终可见，条目加入成功；解锁后点击桌面，窗口隐藏。

## 12. 交互

### 12.1 布局
- 左侧分组栏宽 120 px，选中项用 `theme::accent()` 画下划线。
- 网格：列数按窗口宽度和 `cell_size`（加 5 px 间距）自动计算，格子拉伸到填满窗口；
  行数为 `ceil((最大 slot + 1) / 列数)`，再加一行空槽，方便往后添加。`columns` 只为旧
  配置文件兼容保留，不再参与布局。
- 列表：列数按窗口宽度自动计算（最多 8 列）。`list_columns` 同上，仅为兼容保留。
- 空槽画成空底板，可以作为拖放目标。
- 格子数量超过可视区域时**必须**用 `uniform_list` 做虚拟化。名字单行截断，悬停显示完整名称。
- 失效（`Missing`）的条目显示为灰色并加角标。健康检查在窗口第一次显示后的空闲时间进行，每批 32 个，在专用线程上执行。

### 12.2 操作（按里程碑划分）

| 操作 | 行为 | 里程碑 |
|---|---|---|
| 打开窗口 / 切换分组 | 不自动选中条目（v2.9）；只有过滤框里打了字，才选中第一个匹配项 | M4 |
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
| **M4** | `uwp`；`group`；`command`（`extra.shell` 只能是 `cmd`、`pwsh`、`powershell`、`python`；`extra.script` 作为参数传入；`extra.hidden` 对应 `CREATE_NO_WINDOW`；**第一次运行必须弹窗确认**，确认记录用脚本内容的哈希存进 KV，脚本一改就重新确认）；`on_running = ask`；只读的动态分组（`source = "folder:…" / "recent" / "most_used"`） | 插件宿主、云同步、边缘停靠、从第三方导入（另行立项；细则见 §17.2） |

每个里程碑结束时，在 `docs/launcher.md` 里补充或更新“Quick Launch”一节，记录实际实现和本文的差异。

---

## 17.1 M3 实现细则（v2.6，2026-11-23）

> §17 中 M3 只有一行。本节在核对现有代码后，把它拆成可以直接实现的规定。M3 只做本节列出的内容。

### A. 共享的基础：先把两个函数移出 `nohrs-pages`

CLI（`nohrs-cli`）不能依赖 gpui，也就不能依赖 `nohrs-pages`。它要用到的两段逻辑现在都在 `quick_launch.rs` 里，**先移走，再写 CLI**：

| 现位置 | 新位置 | 说明 |
|---|---|---|
| `quick_launch.rs::item_for_path(path, slot, is_dir)` | `nohrs_core::config::quick_launch::item_for_path` | 纯函数，原样移过去；`nohrs-pages` 改为调用新位置，测试一并移动 |
| `quick_launch.rs::write_atomically` | `nohrs_services::fs::write_atomically(path, contents) -> io::Result<u64>` | 保持语义不变：写 `.tmp` → `sync_all` → 复制出 `.bak` → rename，返回内容哈希；带 `#[allow(clippy::disallowed_methods)]` 并注明原因 |

### B. 设置页“快速启动”（§13 的补充）

- `SettingsSection::QuickLaunch` 放在 `Integrations` 之后，同步修改 `ALL` 的长度和顺序；标签用 `t!("Inicialização rápida")`。
- 写在 `settings/quick_launch.rs` 中，使用 `settings.rs` 现有的 `Group` / `Row` / `Pick` 写法。行标签必须是 `&'static str` 并走 `t!`，这样 settings 现有的搜索（`fold(row.label)`）就能直接搜到，**不要另写搜索**。
- **常驻模式（Standalone）没有设置页**，什么都不用做。
- 页面分四组：

| 组 | 行 | 实现 |
|---|---|---|
| 窗口 | 热键、视图（网格/列表）、列数、格子大小、图标大小、列表列数、失焦自动隐藏、启动后隐藏、锁定（置顶）、程序已运行时（激活/新开）、关闭主窗口后保持运行 | 每项改完调用新增的 `set_window(cx, f: impl FnOnce(&mut WindowSettings))`：`touch()` + `mark_dirty()`；改 `topmost` 还要 `sync_topmost`，改 `hotkey` 还要 `sync_hotkey`。数值用现有的步进或下拉控件，范围与 §4 一致 |
| 分组 | 分组列表：新建、改名、删除、上移、下移 | 新建、改名、删除复用 M2 的 `add_group` / `rename_group` / `delete_group`（含确认和“最后一个不能删”）。新增 `move_group(cx, from, to)`；健康状态和 usage 都按分组 id 记录，排序不需要迁移 |
| 条目 | 先在下拉框选分组，再看条目列表（名称、类型、路径），选中一个条目后显示完整表单 | 表单字段：类型（只能选 app / folder / file / url；未知类型只读显示）、名称、路径（带“浏览…”）、参数、工作目录（带“浏览…”）、以管理员运行、图标、标签（逗号分隔）。保存走新增的 `update_item(cx, group, slot, ItemEdit)`，规则同 `edit_item`（名称和路径不能为空、只读时拒绝、`recheck_health`、路径变化时按 S4 重新判断类型）。`edit_item` 改成调用 `update_item`，不保留两套逻辑 |
| 文件 | 文件路径（只读显示）、“打开 quicklaunch.toml”、“在 LumaFiles 中显示” | 打开文件在后台用 `ShellExecuteExW`（verb 为 NULL）；按钮旁显示提示 `t!("Editar pela interface reescreve o arquivo e remove os comentários.")`。只读状态时，在本组顶部用红字显示原因，并把其他三组的所有控件置灰 |

- **热键录制**：复用 settings 已有的 `RecordShortcut` / `record_key` 机制。录到的 gpui 按键（`ctrl-alt-space`）用新增的纯函数 `gpui_keys_to_hotkey(&str) -> Option<String>` 转成 global-hotkey 的写法（`ctrl+alt+Space`），放在 **`nohrs-core`**（v2.7 更正：v2.6 写的是 `nohrs-launcher/src/hotkey.rs`，但设置页所在的 `nohrs-pages` 不能依赖 `nohrs-launcher`），并写测试，覆盖修饰键顺序、字母、数字、F 键、空格、无法转换的键。录完后通过 `sync_hotkey` 注册：
  - 成功：行尾显示“已注册”；
  - 失败：显示 `hotkey_error`，并保留原来的热键（M1 已实现这一点）；
  - 与 Launcher 的热键相同：显示“和搜索启动器的热键冲突”。
- 设置页和快速启动窗口**读写的是同一个 `QuickLaunch` 全局状态**，任何一边修改，另一边下一帧就能看到（`touch()` + `cx.refresh_windows()`）。

### C. 命令行：`noh quick-launch`（别名 `noh ql`）

| 子命令 | 行为 |
|---|---|
| `list [--json]` | 列出所有分组和条目：分组 id、槽位、类型、名称、路径。第一行打印正在使用的 `quicklaunch.toml` 路径 |
| `add <路径> [--group <id>] [--name <名称>] [--slot <n>] [--args <参数>] [--admin]` | 用 `item_for_path` 判断类型。`--group` 默认取第一个分组；`--slot` 默认取第一个空槽，指定的槽已被占用时报错并以退出码 1 退出，**不覆盖**。文件不存在时报错 |
| `remove <分组id> <槽位>` | 删除条目 |
| `run <分组id> <槽位> [--admin]` | Windows：初始化 COM 后用 `LaunchRegistry::with_builtins()` 启动。文件夹（`OpenFolder`）通过 `noh.exe` 同目录下的 `LumaFiles.exe <路径>` 打开（已运行的实例会接管）；找不到 `LumaFiles.exe` 就报错。非 Windows 平台输出“仅支持 Windows” |
| `check [--json]` | 对每个条目做健康检查（规则同 `LaunchAction::health`），有失效条目时退出码为 1 |

规则：
- 配置路径与 GUI 的解析方式完全相同（`paths::quick_launch_file()`，包括便携模式）。
- 写入前先 `parse`：遇到 `Parse` 或 `UnsupportedVersion` 时**拒绝写入**，打印原因，退出码 2；写入走 `nohrs_services::fs::write_atomically`。
- **CLI 不打开 `state.redb`**（GUI 运行时它被独占锁定）。所以 `remove` 留下的 usage 记录由 GUI 清理（见 E）。
- GUI 正在运行时，CLI 写入的结果由 GUI 的热重载读进去（按 §5.3，以外部文件为准）。
- 输出文字使用英文，与现有 `noh` 子命令保持一致，不走 `t!`。
- 测试放在 `crates/nohrs-cli/tests/quick_launch.rs`：使用临时配置目录（设置绝对路径的 `XDG_CONFIG_HOME`），覆盖 add 后 list、占用槽位报错、remove、解析失败时拒绝写入、`list --json` 的结构。

### D. 搜索启动器里的 `@`（Launcher 数据源）

依赖方向：`nohrs-launcher` 不能依赖 `nohrs-pages`（§3.4），所以用**钩子**连接，写法和 M1 的 `HotkeyHooks` 一样：

```rust
// nohrs-launcher
pub struct QuickLaunchEntry { pub group: SharedString, pub slot: u32, pub name: SharedString,
                              pub tags: Vec<SharedString>, pub kind: SharedString, pub path: SharedString }
#[derive(Clone, Copy)]
pub struct QuickLaunchHooks {
    pub snapshot: fn(&App) -> Vec<QuickLaunchEntry>,                    // 在前台调用，只克隆内存里的数据
    pub activate: fn(&mut App, group: &str, slot: u32, reveal: bool),
}
pub fn set_quick_launch_hooks(cx: &mut App, hooks: QuickLaunchHooks);   // 存成 Global
```

- `nohrs/src/app.rs` 在主模式和常驻模式下都调用 `set_quick_launch_hooks`，实现转发到 `nohrs_pages::quick_launch` 中新增的 `launcher_snapshot` / `activate_by_id`。
- `ranking::ItemKind` 新增 `QuickLaunch` 变体，`badge` 返回 `t!("Inicialização rápida")`，`icon_path` 使用已有的启动类图标。`LauncherItem` 新增 `pub quick_launch: Option<(SharedString, u32)>`；文件搜索结果一律填 `None`。
- **只在查询以 `@` 开头时**搜索快速启动条目（D1）：去掉 `@` 后，在 `schedule_search` 里先在前台取快照，再到后台用 nucleo 匹配名称和标签，按分数、分组顺序、槽位排序，最多返回 `MAX_RESULTS` 条。此时**不搜索文件**。查询不以 `@` 开头时，行为和现在**完全一样**，现有权重一律不改。
- `open_selected`：`quick_launch` 为 `Some` 时调用 `activate(group, slot, false)`，否则保持原行为。`reveal_selected`：调用 `activate(.., true)`，即 Shift+单击时的“打开所在位置”。两种情况都关闭 Launcher 窗口。
- 只有 `@`、后面没有内容时，列出全部条目（按分组和槽位排序），方便浏览。
- placeholder 提示补充 `@`：在现有提示后面加 `t!("@ para a inicialização rápida")`。
- 测试：ranking 测试覆盖 `@` 时只返回快速启动条目、不带 `@` 时结果与改动前完全一致（用现有测试保证）、名称和标签匹配、空 `@` 列出全部；`activate_by_id` 测试覆盖分组 id 不存在时什么都不做。

### E. M2 遗留和补充

1. **`exe,索引` 图标进磁盘缓存**（M2 未决问题 5）：缓存键加入索引号，`split_icon_index` 的结果参与 `icon_cache_file` 的哈希；补一个测试，验证同一个 exe 的不同索引得到不同的缓存文件。
2. **加载时清理无主的 usage 记录**（只在主模式下，因为常驻模式没有 KV）：每次 `apply_loaded` 成功后，在后台列出 `quick_launch` 命名空间里的 `usage_*`，把不对应任何现有（分组 id，槽位）的键用一次 `batch` 删掉。这样 CLI 删除条目或用户手改文件后，新放进同一个槽位的条目不会继承旧的次数。补测试。
3. **`docs/launcher.md`** 补充“Quick Launch”一节（§17 要求每个里程碑都写，M1 和 M2 都没写）：实际的模块位置、配置文件、热键、常驻模式、`@` 搜索，以及与本规格的差异（失败提示改为底部一行、编辑器用窗口内浮层、复制路径等）。
4. **schema**：`QuickLaunchConfig` 及其子类型派生 `JsonSchema`，新增 `crates/nohrs-core/examples/quicklaunch_schema.rs`，输出 `docs/quicklaunch.schema.json`，写法照抄 `examples/schema.rs`；如果现有 schema 有一致性测试，照样加一个。`docs/config.md` 增加一节介绍 `quicklaunch.toml`，并链接到这个 schema。

### F. M3 的决定（BoBO 未另行指定时按这里执行）

| # | 问题 | 做法 |
|---|---|---|
| D1 | `@` 以外的普通搜索，要不要混入快速启动条目 | 不混入。只有 `@` 时搜索，现有排序完全不变 |
| D2 | `noh.exe` 要不要放进安装包 | 不放。M3 的 CLI 只在开发和便携场景下使用；以后要放时另行决定 |
| D3 | 设置页里的条目要不要支持拖动排序 | 不支持。用快速启动窗口的拖动，或在表单里修改槽位 |

### G. M3 验收
- 自动测试：A–E 中列出的全部测试；fmt、clippy 与 main 对比、test 的原始输出。
- 由 BoBO 手测：
  1. 在设置页改热键，新热键立刻生效，旧热键失效；
  2. 设置页改名称和图标后，快速启动窗口立刻更新；
  3. 设置页的修改在 1 秒内写进 `quicklaunch.toml`；
  4. 在 Launcher 里输入 `@ps` 能找到 Photoshop，回车启动；
  5. `noh ql add` 加一个条目，正在运行的快速启动窗口里立刻出现；
  6. 在设置页的搜索框里输入“热键”，能找到快速启动的热键这一行。

## 17.2 M4 实现细则（v2.8，2026-11-23）

> 本节把 §17 里 M4 的那一行拆成可以直接实现的规定，与 §17.1 同级。
> 规格 §1（执行总则）、§3（依赖方向）、§8.4（线程、COM、unsafe）、§18（禁止事项）全部继续有效。
> **M4 只做本节列出的内容。**

### 1. 范围总览

| 编号 | 内容 | 主要位置 |
|---|---|---|
| A | 配置模型：`on_running = "ask"`、分组 `source`、command 的 `extra` 校验、schema | `nohrs-core/src/config/quick_launch.rs` |
| B | `uwp` 类型、已安装应用列表、设置页的应用选择器、`noh ql apps` | `nohrs-services/src/launch/windows.rs`、settings、CLI |
| C | `command` 类型的启动 | `nohrs-services/src/launch/windows.rs` |
| D | command 的首次运行确认与信任记录 | `nohrs-pages/src/quick_launch.rs`、`view.rs`、settings |
| E | `group` 类型：一次启动多个条目 | `nohrs-pages`、settings、CLI |
| F | `on_running = "ask"`：程序已在运行时询问 | services、pages、settings |
| G | 动态分组：`folder:` / `recent` / `most_used`（只读） | `nohrs-pages`、settings、CLI、launcher 快照 |
| H | 文档与翻译 | `docs/launcher.md`、`docs/config.md`、i18n |

**不做**：插件宿主、云同步、贴边停靠、从第三方工具导入（与 §17 的 M4 行一致）。

---

### A. 配置模型（`nohrs-core`）

1. **`OnRunning::Ask`**：新增变体，序列化为 `"ask"`。默认值仍是 `activate`。
2. **分组来源**：`Group` 新增字段
   ```rust
   #[serde(default, skip_serializing_if = "Option::is_none")]
   pub source: Option<String>,
   ```
   以及解析函数 `pub fn group_source(group: &Group) -> Option<GroupSource>`：
   ```rust
   pub enum GroupSource { Folder(String), Recent, MostUsed, Unknown(String) }
   ```
   规则：
   - `folder:<路径>`：冒号后面的原样保留，路径变量在使用时再展开；
   - `recent`、`most_used`；
   - 其他写法记为 `Unknown`，并给出 warning；
   - 设置了 `source` 的分组，如果文件里还写了 `[[group.item]]`，这些条目**丢弃**，并给出 warning `Warning::ItemsInDynamicGroup { group }`；
   - `to_toml` 输出动态分组时**只写 `id`、`name`、`source`**，不写运行时生成的条目（由 G 节保证内存里有条目也不会写出去）。
3. **command 的 `extra`**：新增 `pub struct CommandSpec { shell: Shell, script: String, hidden: bool, keep_open: bool }` 和 `enum Shell { Cmd, Pwsh, PowerShell, Python }`，以及 `pub fn command_spec(item: &Item) -> Result<CommandSpec, String>`：
   - `shell` 必须是 `cmd` / `pwsh` / `powershell` / `python` 之一；
   - `script` 必须是非空字符串；
   - `hidden`、`keep_open` 可省略，默认 `false`，写了就必须是布尔值；
   - 出现其他键时给出 warning `Warning::UnknownCommandKey`，条目保留。
4. **schema**：重新生成 `docs/quicklaunch.schema.json`，一致性测试必须通过。
5. **测试**：`ask` 往返序列化；三种 source 和 Unknown；动态分组的条目被丢弃并给出 warning；`to_toml` 不输出动态分组的条目；`command_spec` 的每条规则。

---

### B. `uwp` 类型

1. **约定**：`path` 存 AppUserModelID（例如 `Microsoft.WindowsCalculator_8wekyb3d8bbwe!App`），`args` 作为启动参数，`cwd` 和 `admin` 不适用（设置了也忽略，设置页里置灰）。
2. **启动**：在 `launch/windows.rs` 新增 `UwpAction`，用 `CoCreateInstance(ApplicationActivationManager)` 拿到 `IApplicationActivationManager`，调用 `ActivateApplication(aumid, args, AO_NONE)`。调用发生在 `quick-launch` 线程上，那里已经进入了 COM apartment。
   - 成功返回 `Started`；
   - 失败返回 `Os { code, message }`，并沿用 M1 的 HRESULT 换算。
   - 需要追加的 `windows` feature 只能加到 `nohrs-services`，版本保持 0.61。
   - `unsafe` 只能出现在这个文件里，每处都要写 `// SAFETY:`，句柄和接口使用 RAII 管理。
3. **健康检查**：只做格式检查：`!` 前后都非空，不含空白字符，长度不超过 256，否则返回 `Invalid`。**不查询系统**，因为 §8.1 规定健康检查不能超过 5 ms。
4. **已安装应用列表**：`pub fn installed_apps() -> Result<Vec<InstalledApp>, LaunchError>`，其中 `InstalledApp { name: String, aumid: String }`。
   - 实现：`SHGetKnownFolderItem(FOLDERID_AppsFolder)`，再用 `BindToHandler(BHID_EnumItems)` 枚举，每一项读取显示名和 `PKEY_AppUserModel_ID`。
   - 列表里也会包含经典桌面程序（它们同样有 AUMID），一并返回，`ActivateApplication` 对两者都适用。
   - 只能在后台线程调用（耗时可能达到数百毫秒），调用方负责进入 COM。
   - 非 Windows 平台返回 `Unsupported`。
5. **设置页**：类型下拉框加入 `uwp`。选中 `uwp` 后，“路径”一行改名为 `t!("ID do aplicativo")`（中文：应用 ID），旁边的“浏览…”换成“选择应用…”：
   - 点击后在后台调用 `installed_apps()`，弹出一个带搜索框的列表（按名称过滤）；
   - 选中后填入 AUMID，名称为空时一并填入应用名；
   - 列表结果在本次会话内缓存，避免重复枚举。
6. **命令行**：新增 `noh ql apps [--json]`，输出名称和 AUMID。`noh ql add` 新增参数 `--uwp <AUMID>`，与 `<路径>` 二选一。
7. **测试**：AUMID 格式检查（合法、缺 `!`、两侧为空、含空格、超长）；`installed_apps` 标记 `#[ignore]`，真机运行时至少返回一项；CLI 的 `apps --json` 输出结构测试（Windows 下运行）。

---

### C. `command` 类型的启动（`nohrs-services`）

1. 新增 `CommandAction`，只在 Windows 上注册。用 `std::process::Command` 启动，**不写 unsafe**：

| shell | 程序 | 参数 |
|---|---|---|
| `cmd` | `%ComSpec%`，没有就用 `cmd.exe` | `/d /s /c` 再加 `raw_arg("\"<script>\"")`；`keep_open` 时把 `/c` 换成 `/k` |
| `pwsh` | `pwsh.exe` | `-NoLogo -NoProfile -EncodedCommand <base64(UTF-16LE(script))>`；`keep_open` 时在 `-EncodedCommand` 之前加 `-NoExit` |
| `powershell` | `powershell.exe` | 同 pwsh |
| `python` | `python.exe` | `keep_open` 时加 `-i`，然后加 `-c` 和 `.arg(script)` |

   - PowerShell 用 `-EncodedCommand`，是为了**彻底避开命令行引号转义**；cmd 用 `/s /c "…"` 的标准写法；python 用 `.arg()`，转义交给 Rust。
   - `hidden = true` 时设置 `CREATE_NO_WINDOW`；否则设置 `CREATE_NEW_CONSOLE`，让脚本有自己的控制台窗口。
   - 工作目录：`cwd` 展开后的路径；没设置就用 `%USERPROFILE%`。
   - 进程启动后**不等待**，返回 `Started`。启动失败（找不到程序等）返回 `NotFound` 或 `Os`。
2. **`admin = true` 的 command 本期不支持**（D4）：健康检查返回 `Invalid(t!(…))`，启动返回 `Unsupported`，设置页在类型为 command 时把“以管理员运行”置灰。
3. **services 层不负责确认**：确认在 pages 层（D 节）。services 只提供 `CommandAction`，调用方必须先完成确认。这一点要写在 `CommandAction` 的文档注释里。
4. **健康检查**：`command_spec` 报错就返回 `Invalid`；设置了 `cwd` 但目录不存在就返回 `Missing`；不检查 shell 程序是否在 PATH 上（太慢）。
5. **测试**：参数拼装写成纯函数 `command_line(spec) -> (program, Vec<Arg>)`，每种 shell、`keep_open`、`hidden` 的组合都要有测试；`-EncodedCommand` 的编码对含中文和引号的脚本往返一致；`#[ignore]` 的真机测试：cmd 脚本 `echo ok > %TEMP%\ql-test.txt` 能写出文件。

---

### D. 首次运行确认与信任记录（`nohrs-pages`）

1. **信任键**：`trust_key(item, spec) = sha256(shell \0 script \0 cwd \0 hidden \0 keep_open)` 的前 32 位十六进制，KV 名为 `trusted_<32 位十六进制>`。用 pages 已有的 `sha2`（**不要**用 `DefaultHasher`：这里关系到安全，必须抗碰撞）。
2. **何时需要确认**：要启动的 command 条目，在 KV 里查不到它的信任键时，就需要确认。M2 已经让 KV 的读取在前台走内存缓存；这里同样在 `restored` 里缓存所有 `trusted_*`，前台只查内存，**不读 redb**。
3. **确认对话框**：沿用 M2 编辑器那种窗口内浮层的做法，内容包括：
   - 条目名称、shell、工作目录；
   - 是否隐藏窗口，如果是，用醒目的颜色提示“将在后台运行，看不到窗口”；
   - **完整脚本**：等宽字体，可以滚动，最大高度为窗口的 60%；
   - 按钮“运行”和“取消”。
   - 文案：标题 `t!("Executar este comando?")`，说明 `t!("Este comando roda com as suas permissões. Confira o conteúdo antes de continuar.")`。

   点击“运行”后写入信任键，然后启动；点击“取消”什么都不做，也不报错。
4. **窗口隐藏时**（例如从 Launcher 的 `@` 启动）：先调用 `show()` 显示快速启动窗口，再弹出确认框。
5. **常驻模式**（没有 KV）：信任只保存在内存里，进程退出就失效；对话框底部加一句 `t!("Sem armazenamento: a confirmação vale até fechar o programa.")`。
6. **在设置页里保存的脚本自动信任**（D5）：通过设置页表单保存 command 条目时，把新的信任键写入 KV。手改文件、用命令行修改、热重载读进来的变化**都不会**自动信任。
7. **撤销**：设置页的 command 表单显示“已信任 / 未信任”，并提供“撤销信任”按钮，删除对应的 KV 键。
8. `prune_usage` 只清理 `usage_*`，**不能**碰 `trusted_*`。补一个测试保证这一点。
9. **测试**：信任键的稳定性（同样输入得到同样的键，改一个字符就变）；未信任时不发送启动请求，而是打开确认；确认后写入 KV 并启动；改了脚本后需要重新确认；设置页保存后自动信任；撤销信任；常驻模式只在内存里信任；`prune_usage` 不删除 `trusted_*`。

---

### E. `group` 类型：一次启动多个条目

1. **执行位置**：pages 层。`activate_with` 遇到 `group` 条目时，**不**发送给 registry，而是：
   1. 按 `items` 里的 id，在**同一分组**中依次找到被引用的条目；遇到嵌套的 group 条目就递归展开，深度不超过 8，已经访问过的跳过；展开后最多 16 个条目，多出的不启动，并在底部提示中说明；
   2. 收集其中所有**未信任**的 command 条目，如果有，弹出**一个**确认框，把它们一起列出来；取消就整组都不启动；
   3. 按顺序给每个条目发送启动请求，共用一个“批次 id”。`on_running = ask` 在分组里一律按 `activate` 处理，不逐个询问（D9）；
   4. 整批完成后：
      - 有失败的，底部显示 `tf!("{failed} de {total} itens não iniciaram: {names}")`；
      - 有 `OpenFolder` 的，逐个在资源管理器里打开；
      - 使用次数**只记在 group 条目本身**，不记在各个子条目上；
      - `close_after_launch` 在整批完成后才判断一次是否隐藏。
2. **健康检查**：没有任何有效引用时返回 `Invalid`；否则返回 `Ok`。子条目各自的健康状态照常显示。
3. **设置页**：类型下拉框加入 `group`。选中后，路径、参数、工作目录都隐藏，换成“包含的条目”多选列表（同一分组中除自己以外的条目，按槽位排序，显示勾选顺序）。保存时，**被选中但还没有 id 的条目自动分配 id**（`item_<槽位>`，冲突时追加 `_2`、`_3`……），整个修改作为一次保存写入。
4. **快速启动窗口里的编辑浮层**：group 条目只能修改名称，其余字段提示“请在设置页中编辑”。
5. **命令行**：`noh ql run` 能运行 group，按上面的规则依次启动；遇到 command 子条目时必须带 `--yes`，否则整组拒绝执行，退出码为 1（D6）。
6. **测试**：引用解析（顺序、嵌套、环、超过 16 个、跨组引用被忽略）；未信任的子条目汇总到一个确认框；部分失败时的提示文本；使用次数只记在 group 上；设置页保存时自动分配 id 且不冲突。

---

### F. `on_running = "ask"`

1. **services**：`LaunchOutcome` 新增 `AlreadyRunning { process_id: u32 }`。`AppAction` 在 `on_running == Ask` 并且找到正在运行的进程时返回它，**不激活、也不启动**。另外新增一个只供 pages 使用的入口：`LaunchMode::Activate(process_id)`，表示激活这个进程的主窗口；找不到窗口就退回到启动新实例。
2. **pages**：收到 `AlreadyRunning` 后弹出窗口内浮层，提示 `t!("Este programa já está aberto.")`，按钮为：
   - “切换到它”：用 `LaunchMode::Activate(pid)` 再发一次请求；
   - “再开一个”：用 `LaunchMode::ForceNew` 再发一次请求；
   - “取消”。

   窗口隐藏时先显示窗口。使用次数在用户选择之后才记录；选“取消”不计数。
3. **设置页**：“程序已在运行时”的选项增加“每次询问”。
4. **命令行**：`ask` 按 `activate` 处理（命令行没有界面）。
5. **测试**：services 中 ask 返回 AlreadyRunning（用注入的进程查找函数测试，不依赖真实进程）；pages 中三个按钮各自发出的请求；取消时不计数。

---

### G. 动态分组（只读）

1. **运行时条目**：
   - 动态分组的条目只存在于内存中的 `config.groups[i].items`，在每次 `apply_loaded` 之后以及刷新时重新生成。
   - `save_now` / `save_in_background` 在 `to_toml` 之前，**先复制一份配置，并把动态分组的条目清空**（A.2 再兜一层底）。
   - 状态中新增 `origins: HashMap<(String, u32), (String, u32)>`，记录 `(动态分组 id, 槽位)` 对应的 `(原分组 id, 原槽位)`，只有 `recent` 和 `most_used` 会用到。
2. **`folder:<路径>`**：
   - 在后台展开路径并读取目录，**不递归**；按名称排序（不区分大小写），每一项用 `item_for_path` 生成条目，槽位从 0 开始连续编号，最多 512 项，跳过隐藏文件和系统文件。
   - 刷新时机：窗口每次显示时，以及切换到该分组时，距离上次刷新超过 30 秒才刷新。**不使用**文件监视。
   - 目录不存在时，分组显示 `t!("Pasta não encontrada")`，没有条目。
3. **`recent` / `most_used`**（只在主模式下有效，常驻模式下没有 KV，分组显示 `t!("Indisponível no modo residente")`）：
   - 从内存中的使用记录（`restored`，必要时在后台读取 KV）里，按 `last` 从新到旧排序（recent），或按 `count` 从多到少排序，次数相同时按 `last` 排序（most_used）；
   - 只保留在当前配置中**仍然存在**的原条目，排除动态分组自己；
   - 最多 `columns × 2` 项，不超过 32 项（D7）；
   - 复制原条目，槽位从 0 开始编号，并在 `origins` 中记录来源；
   - 刷新时机：窗口每次显示时，以及每次启动成功后。
4. **启动和记录**：在 `recent` / `most_used` 分组里启动条目时，使用次数和健康检查都按 `origins` 记到**原条目**上。
5. **只读**：
   - 新增 `group_editable(cx, index) = editable(cx) && source.is_none()`。`place_items`、`put_item`、`edit_item`、`update_item`、`swap_slots`、`delete_item` 都要检查它；`move_item_to_group` 的来源分组和目标分组只要有一个是动态分组就拒绝。
   - 快速启动窗口中，动态分组的拖入、拖动、右键编辑都要置灰；“打开所在位置”和“复制路径”仍然可用。
   - 动态分组本身可以**改名、删除、调整顺序**。
6. **设置页**：
   - 新建分组时可以选择“普通 / 文件夹 / 最近使用 / 最常使用”，选择文件夹时用 `prompt_for_paths` 选取目录；
   - 分组列表里显示来源；
   - 动态分组的条目列表只读，并显示 `t!("Itens gerados automaticamente; não podem ser editados.")`。
7. **Launcher 快照**：`launcher_snapshot` 包含 `folder:` 分组的条目，**排除** `recent` / `most_used` 分组（它们的条目都是别处条目的副本，包含进来会重复）。
8. **prune_usage**：计算“有效条目”时排除动态分组，避免把它们当成有效条目、从而留下不该留的使用记录。
9. **命令行**：`list` 中动态分组标注 `(dynamic: <source>)`，只显示文件里写的内容（没有条目）；`add`、`remove` 遇到动态分组时拒绝，退出码为 1；`run` 遇到动态分组时报错，提示“请在界面中使用”。
10. **测试**：`folder:` 生成条目的规则（排序、跳过隐藏文件、512 上限、目录不存在）；recent 和 most_used 的排序、上限、排除已删除的原条目；`origins` 让使用次数记到原条目上；保存时不写出动态分组的条目；所有编辑入口都拒绝修改动态分组；Launcher 快照的包含和排除规则；prune_usage 排除动态分组。

---

### H. 文档与翻译

- `docs/launcher.md` §14：补充 uwp、command（确认和信任）、group、ask、动态分组，以及与规格的差异。
- `docs/config.md`：补充 `source`、`extra`（command）、`ask` 的说明和示例。
- 所有新增文案都用 `t!`，9 种语言补齐，`every_key_in_the_code_has_a_translation` 测试必须通过。

---

### M4 的决定（D4、D5、D7 已由 BoBO 于 2026-11-23 确认；D6、D8、D9 按默认执行）

| # | 问题 | 做法 |
|---|---|---|
| D4 | command 以管理员身份运行 | ✅ **已确认**：本期不支持，设置页置灰 |
| D5 | 在设置页保存的脚本是否自动信任 | ✅ **已确认**：是。只有设置页保存会自动信任，手改文件、命令行修改都不会 |
| D6 | 命令行运行 command | 必须带 `--yes`，否则拒绝 |
| D7 | 最近使用 / 最常使用分组的数量 | ✅ **已确认**：`columns × 2`，最多 32 项 |
| D8 | command 的 `keep_open` 选项 | 提供，默认关闭 |
| D9 | 分组启动时遇到 `ask` | 按 `activate` 处理，不逐个询问 |

---

### 提交顺序

0 规格 v2.8 → A → B → C → D → E → F → G → H。每项一个提交；翻译可以随各自的项一起提交。

### 验收

- **原始输出**：
  - `cargo fmt --all -- --check`
  - **逐包** `--no-deps` clippy（`nohrs-core`、`nohrs-services`、`nohrs-launcher`、`nohrs-pages`、`nohrs-cli`），main 与本分支各跑一次，比较结果
  - `cargo build --release -p nohrs`，检查有没有 rustc 警告
  - 逐包测试，已知会失败的日志测试单独列出
  - 每个提交的哈希和改动的文件
  - 重新编译 `LumaFiles.exe` 和 `noh.exe` 的时间
  - 未决问题
- **BoBO 手测**：
  1. 用设置页的“选择应用…”添加“计算器”（uwp），点击后能启动；
  2. 新建一个 command：`cmd`，脚本 `echo hello & pause`。第一次点击弹出确认框，确认后弹出控制台；第二次点击直接运行；用记事本改 toml 里的脚本后，再次点击需要重新确认；
  3. 设一个 `hidden = true` 的 command，确认框里有醒目的“后台运行”提示；
  4. 新建一个 group，包含 3 个条目，点击后 3 个都启动，`close_after_launch` 只在最后隐藏一次窗口；
  5. 把 `on_running` 设为“每次询问”，在程序已经打开的情况下点击它，三个按钮的行为都正确；
  6. 新建一个“文件夹”分组，指向你常用的工具目录，里面的程序都显示出来，而且不能拖入、不能编辑；
  7. “最近使用”分组按顺序显示刚刚启动过的条目，在这个分组里启动的次数记到原条目上；
  8. 在 Launcher 里输入 `@`，列表中不会出现“最近使用”里的重复条目。

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
