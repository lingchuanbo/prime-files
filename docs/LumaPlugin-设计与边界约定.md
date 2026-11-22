# LumaFiles 插件系统（对齐 Total Commander WFX / WLX / WDX）设计与边界约定 · v1

> 状态：草案 v1.1。§17 的 Q2、Q3、Q5 已由 BoBO 确认（2026-11-22），其余问题按默认做法执行
> 读者：实现方（DeepSeek）和 BoBO。插件作者请看 `docs/LumaPlugin-SDK开发指南.md`。
> 决策记录：[ADR 0010](./adr/0010-tc-plugin-kinds-and-native-bridge.md)。兼容性记录：[`plugin-compat.md`](./plugin-compat.md)。
> 关系：本文**不推翻** ADR 0005（WASM Component Model 作为原生插件路线），而是在它之上补两件事：
> 1. 插件按 TC 的三类划分：文件系统、查看器、内容；
> 2. 新增 TC 原生插件兼容层，可以直接加载现有的 **64 位** `.wfx64 / .wlx64 / .wdx64`（Q3：**不支持 32 位插件**）。
>
> 对齐基准（官方 SDK）：文件系统插件 **2.1 SE**、查看器插件 **2.13**、内容插件 **2.12**。
> 来源：<https://ghisler.github.io/WFX-SDK/contents.htm>、<https://ghisler.github.io/WLX-SDK/contents.htm>、<https://ghisler.github.io/WDX-SDK/contents.htm>
>
> 文中凡写“必须 / 禁止”的，实现方不得自行变更；没写到的情况按 §16 处理。

---

## 0. 结论先行

| 决策 | 内容 | 理由 |
|---|---|---|
| D1 两条插件路线 | **原生插件**：WASM 组件，沙箱运行，跨语言；**兼容插件**：TC 原生 DLL，进程外加载 | 原生路线安全、可移植；兼容路线让 TC 现有的成千上万个插件直接可用 |
| D2 一套宿主抽象 | 界面层只认 `FsProvider / Viewer / ContentProvider` 三个 trait，下面有两个后端：`TcBackend` 和 `WasmBackend` | 资源管理器、批量重命名、搜索不用关心插件来自哪条路线 |
| D3 TC 插件**进程外**运行 | 由 `luma-tcbridge.exe`（只有 x64 版本）加载 DLL，**默认每个 DLL 一个桥接进程**，通过命名管道通信 | 主程序 `unsafe_code = deny`；第三方 DLL 崩溃、卡死、内存泄漏都不能拖垮 LumaFiles |
| D4 检测字符串用 TC 语法 | 宿主统一解析 `EXT="JPG" \| SIZE<1000 \| FIND("PK")` 这类表达式，两条路线共用 | 和 TC 一致，用户从 TC 导入的关联规则不用改 |
| D5 配置跟着 LumaFiles 走 | 新增 `plugins.toml`，放在当前配置目录（便携模式下在 `userdata/`）；TC 插件的 ini 统一放在 `{config}/plugins/tc/` | 和 `quicklaunch.toml` 等同一套机制，便携模式可整体搬走 |
| D6 先做内容插件，再做查看器，最后做文件系统 | 改动面由小到大：内容插件只加列，查看器要嵌窗口，文件系统要引入虚拟路径 | 每一步都能单独交付、单独验收 |
| D7 WASM 原生路线放在 TC 兼容之后 | 三类 WIT 接口现在定稿（见 SDK 文档），运行时（wasmtime）在 P4 实现 | 兼容层能立刻带来价值；WIT 先定稿，插件作者可以提前准备 |

---

## 1. 术语对照

| TC | LumaFiles 界面名 | 代码名 | TC 后端 | WASM 后端 |
|---|---|---|---|---|
| 文件系统插件 (.WFX) | 文件系统插件 | `fs` / `FsProvider` | `.wfx64`（以及 PE 头为 x64 的 `.wfx`） | world `fs-plugin` |
| 查看器插件 (.WLX) | 查看器插件 | `viewer` / `Viewer` | `.wlx64`（同上） | world `viewer-plugin` |
| 内容插件 (.WDX) | 内容插件 | `content` / `ContentProvider` | `.wdx64`（同上） | world `content-plugin` |
| 网上邻居 `\\\` | 插件根 | `plugin root` | — | — |
| Lister（F3） | 查看器窗口 | `ViewerWindow` | — | — |
| 快速查看（Ctrl+Q） | 预览面板 | `preview` | — | — |
| 自定义列 | 列集 | `ColumnSet` | — | — |
| 压缩包插件 (.WCX) | 压缩包插件 | **P6**（Q5），本期不做 | — | — |

---

## 2. 总体架构

```text
┌──────────────────────────── LumaFiles.exe（主进程）────────────────────────────┐
│  nohrs-pages：资源管理器 / 预览面板 / 查看器窗口 / 批量重命名 / 搜索 / 设置页  │
│        │  只依赖下面的 trait，不知道插件来自哪条路线                          │
│        ▼                                                                       │
│  nohrs-plugins（新 crate）                                                     │
│    registry.rs   插件注册表、启用/禁用、优先级                                  │
│    detect.rs     TC 检测字符串解析与求值（纯逻辑）                               │
│    config.rs     plugins.toml 模型、校验、迁移                                   │
│    fs.rs / viewer.rs / content.rs    三个 trait 和数据类型                       │
│    content_cache.rs                  内容字段缓存与延迟取值队列                    │
│    tc/ （TcBackend）  启动、监控、重启桥接进程；命名管道客户端                   │
│    wasm/（WasmBackend，P4）  调用 nohrs-plugin-host                              │
└───────────────┬──────────────────────────────────────────────┬────────────────┘
                │ 命名管道（每个插件一条）                     │ 进程内（P4）
                ▼                                              ▼
┌──────────────── luma-tcbridge.exe（x64）──────────────┐  ┌── nohrs-plugin-host ──┐
│  nohrs-tcbridge（新 crate，bin）                     │  │  wasmtime + WASI P2   │
│   LoadLibraryW + GetProcAddress                       │  │  三个 world           │
│   UI 线程：消息循环，查看器窗口、ListLoadW            │  └───────────────────────┘
│   调用线程：串行化所有插件调用                        │
│   后台传输线程：仅当 FsGetBackgroundFlags 允许         │
│   回调桥：Progress / Log / Request / Crypt → 主进程    │
└───────────────────────────────────────────────────────┘
```

### 2.1 crate 划分

| crate | 类型 | 新增或改动 | 平台 | 说明 |
|---|---|---|---|---|
| `nohrs-tcbridge-proto` | lib | **新增** | 全平台可编译 | 管道帧格式和消息枚举。只依赖 `serde`、`serde_json`。主进程和桥接进程共用 |
| `nohrs-plugins` | lib | **新增** | 全平台可编译；`tc/` 模块 `#[cfg(windows)]` | trait、注册表、检测字符串、配置、缓存、TcBackend |
| `nohrs-tcbridge` | bin | **新增** | 只支持 Windows x64 | 产出 `luma-tcbridge.exe`。**不做** x86 构建（Q3） |
| `nohrs-plugin-host` | lib | P4 新增 | 全平台 | 按 ADR 0005 实现，本文只规定它要实现的三个 world |
| `nohrs-pages` | lib | 改动 | — | 接入点见 §7–§9、§12 |
| `nohrs-core` | lib | 改动 | — | `paths.rs` 新增 `plugins_file()`、`plugins_dir()`、`tc_ini_dir()`；i18n 文案 |
| `nohrs-services` | lib | 改动（P3） | — | 新增 `vfs.rs`：本地路径和插件路径之间的复制、移动 |

- 依赖方向：`pages → plugins → (tcbridge-proto, core)`；`tcbridge → tcbridge-proto`。**禁止** `tcbridge` 依赖 `core` 及以上任何 crate：桥接进程要保持小巧、启动快，而且不能把 gpui 等依赖带进第三方 DLL 所在的进程。
- 新 crate 不进入 `default-members`（与 GUI crate 同样处理），但 `nohrs-tcbridge-proto` 和 `nohrs-plugins` 中的纯逻辑模块必须在 Linux CI 上通过 `cargo test -p`。
- `unsafe` 只允许出现在：`nohrs-tcbridge`（整个 crate 加 `#![allow(unsafe_code)]`，每处写 `// SAFETY:`），以及 `nohrs-pages` 中新增的 `plugin_slot.rs`（嵌入窗口用，逐项 allow）。

---

## 3. 宿主抽象（`nohrs-plugins`）

> 所有方法都是**同步签名，在后台线程调用**。界面线程只能通过 `cx.background_spawn` 或插件专用工作队列调用，**禁止**在 gpui 前台线程直接调用任何插件方法。

### 3.1 公共类型

```rust
pub struct PluginId(String);              // plugins.toml 里的 id，[a-z0-9_]{1,32}
pub enum Backend { Tc, Wasm }        // Q3：TC 后端只有 x64，不区分位数

pub struct PluginInfo {
    pub id: PluginId,
    pub name: String,                     // 显示名
    pub kinds: Kinds,                     // bitflags: FS | VIEWER | CONTENT（一个 DLL 可同时是 WFX 和 WDX）
    pub backend: Backend,
    pub version: Option<String>,
    pub detect: Option<DetectExpr>,       // 生效的检测字符串：用户覆盖值，或插件自带值
    pub state: PluginState,               // Ready | Starting | Crashed { count, last } | Disabled | Missing
}

#[derive(thiserror::Error)]
pub enum PluginError {
    NotSupported, NotFound, Exists, AccessDenied, Aborted,
    Timeout { call: &'static str, after: Duration },
    Crashed,                              // 桥接进程退出
    Io(String), Network(String), AuthRequired, Other(String),
}
```

### 3.2 文件系统 `FsProvider`

```rust
pub trait FsProvider: Send + Sync {
    fn root_name(&self) -> &str;
    fn capabilities(&self) -> FsCaps;     // GET | PUT | RENAME | COPY_WITHIN | DELETE | MKDIR | RMDIR | ATTRS | TIMES | EXECUTE | BG_GET | BG_PUT | PREVIEW | ICONS | RESUME | FIELDS
    fn list(&self, path: &PluginPath, cancel: &CancelToken) -> Result<Vec<VEntry>, PluginError>;
    fn get_file(&self, remote: &PluginPath, local: &Path, flags: CopyFlags, info: &VEntry, progress: &dyn Progress) -> Result<(), PluginError>;
    fn put_file(&self, local: &Path, remote: &PluginPath, flags: CopyFlags, progress: &dyn Progress) -> Result<(), PluginError>;
    fn transfer_within(&self, from: &PluginPath, to: &PluginPath, mv: bool, overwrite: bool, info: &VEntry) -> Result<(), PluginError>;
    fn delete(&self, path: &PluginPath) -> Result<(), PluginError>;
    fn make_dir(&self, path: &PluginPath) -> Result<(), PluginError>;
    fn remove_dir(&self, path: &PluginPath) -> Result<(), PluginError>;
    fn set_attrs(&self, path: &PluginPath, attrs: u32) -> Result<(), PluginError>;
    fn set_times(&self, path: &PluginPath, times: FileTimes) -> Result<(), PluginError>;
    fn execute(&self, path: &PluginPath, verb: Verb) -> Result<ExecResult, PluginError>;  // Verb: Open | Properties | Chmod(String) | Command(String)
    fn disconnect(&self, root: &PluginPath) -> Result<(), PluginError>;
    fn status(&self, path: &PluginPath, phase: StatusPhase, op: StatusOp);              // 对应 FsStatusInfo，失败只记日志
    fn icon(&self, path: &PluginPath, size: u32) -> Option<IconResult>;
    fn preview(&self, path: &PluginPath, w: u32, h: u32) -> Option<ImageRgba>;
    fn content(&self) -> Option<&dyn ContentProvider>;                                  // FsContent* 字段
}
pub enum ExecResult { Done, OpenLocally, Navigate(String) }   // FS_EXEC_OK / FS_EXEC_YOURSELF / FS_EXEC_SYMLINK；FS_EXEC_ERROR 映射为 Err
```

`PluginPath` 是插件内部路径：以 `\` 开头、用 `\` 分隔，与 TC 一致；根目录是 `\`。

### 3.3 查看器 `Viewer`

```rust
pub trait Viewer: Send + Sync {
    fn detect(&self) -> Option<&DetectExpr>;
    fn open(&self, file: &Path, opts: ViewOptions, surface: Surface) -> Result<Box<dyn ViewerSession>, PluginError>;
    fn thumbnail(&self, file: &Path, head: &[u8], w: u32, h: u32) -> Option<ImageRgba>;   // ListGetPreviewBitmap
}
pub enum Surface {
    NativeChild { parent_hwnd: isize },   // TC 后端：插件在这个父窗口下创建子窗口
    Rendered,                             // WASM 后端：插件返回文档模型，由 LumaFiles 绘制
}
pub trait ViewerSession: Send {
    fn load_next(&mut self, file: &Path) -> Result<bool, PluginError>;                    // ListLoadNext；false 表示插件不能复用，需要重新 open
    fn set_options(&mut self, opts: ViewOptions);                                         // lc_newparams
    fn search(&mut self, text: &str, flags: SearchFlags) -> Result<bool, PluginError>;    // ListSearchText
    fn copy(&mut self) -> Result<(), PluginError>;                                        // lc_copy
    fn select_all(&mut self) -> Result<(), PluginError>;                                  // lc_selectall
    fn print(&mut self) -> Result<(), PluginError>;                                       // ListPrint（P2 可不做）
    fn bounds_changed(&mut self, rect: Rect);
    fn document(&self) -> Option<RenderedDoc>;                                            // 仅 Rendered
}
pub struct ViewOptions { wrap: bool, fit: bool, fit_larger_only: bool, center: bool, dark: bool, encoding: TextEncoding }
```

### 3.4 内容 `ContentProvider`

```rust
pub trait ContentProvider: Send + Sync {
    fn detect(&self) -> Option<&DetectExpr>;
    fn fields(&self) -> &[FieldDef];                       // 首次加载后缓存，插件重启后才刷新
    fn value(&self, file: &Path, field: u16, unit: u16, allow_slow: bool) -> FieldResult;
    fn fulltext(&self, file: &Path, field: u16, offset: u64) -> Result<Option<String>, PluginError>;
    fn set_value(&self, file: &Path, field: u16, unit: u16, value: FieldValue, batch: SetBatch) -> Result<(), PluginError>;
    fn stop(&self, file: &Path);                           // ContentStopGetValue
    fn state(&self, event: ContentState, dir: &Path);      // ContentSendStateInformation：进入新目录、刷新、显示提示
    fn default_sort(&self, field: u16) -> SortOrder;
}
pub struct FieldDef { pub name: String, pub kind: FieldKind, pub units: Vec<String>, pub choices: Vec<String>, pub flags: FieldFlags }
pub enum FieldKind { Int32, Int64, Float, Date, Time, DateTime, Bool, Choice, Text, FullText }   // ft_string / ft_stringw 合并为 Text，ft_fulltext / ft_fulltextw 合并为 FullText
pub enum FieldResult { Value(FieldValue), Empty, Delayed, OnDemand, NoSuchField, FileError, NotSupported, Err(PluginError) }
```

---

## 4. 检测字符串（`detect.rs`，纯逻辑）

### 4.1 语法（与 TC 对齐）

```text
expr    := or
or      := and ( '|' and )*
and     := unary ( '&' unary )*
unary   := '!' unary | '(' expr ')' | cmp | 'FORCE' | 'MULTIMEDIA'
cmp     := operand op literal | 'FIND(' string ')' | 'FINDI(' string ')'
operand := 'EXT' | 'SIZE' | '[' number ']'
op      := '=' | '!=' | '<' | '>' | '<=' | '>='
literal := string | number
```

- `EXT` 不区分大小写，不带点。`SIZE` 单位为字节。`[n]` 取文件第 n 个字节，可以和数字或单字符字符串比较。
- `FIND` 和 `FINDI`（不区分大小写）在文件开头 **8 KB** 内查找；读不到 8 KB 就按实际长度查。**实现前请对照 WLX SDK 的 ListGetDetectString 一页核实这个长度**。
- `FORCE`：只有用户在查看器里手动按“插件”或数字键切换插件时才匹配，自动选择查看器时不匹配。
- `MULTIMEDIA`：TC 用它表示“交给内置多媒体查看器”。LumaFiles 映射为“文件属于内置预览支持的媒体类型”，也就是 `preview.rs` 能处理的图片、音视频和 3D 文件。
- 宽松处理：关键字不区分大小写；字符串用双引号；遇到无法解析的字符串，该插件按“空检测字符串”处理，在设置页里标红，**不阻止插件加载**。
- 空检测字符串的语义：查看器插件表示“对所有文件都尝试”，与 TC 一致；内容插件表示“对所有文件提供字段”。

### 4.2 求值
- 只有在表达式确实用到 `[n]` 或 `FIND` 时才读文件头部，读取放在后台线程。
- 同一个文件的头部在一次导航内缓存一份，多个插件共用。
- 必须有单元测试：语法的每条规则、运算优先级（`&` 高于 `|`）、读到半截文件、空文件、非法输入。另外从 TC 常见插件的 `pluginst.inf` 里摘 20 条真实检测字符串，作为回归测试样本。

---

## 5. TC 兼容层

### 5.1 进程模型
- **一个 DLL 一个桥接进程**（`[bridge] per_plugin_process = true`，默认值）。同一个 DLL 同时是 WFX 和 WDX 时，共用同一个进程（这是 TC 的行为，插件内部可能共享全局状态）。
- **懒启动**：第一次需要用到时才启动。查看器插件空闲 10 分钟后退出；内容插件在当前目录还在用到它的列时常驻；文件系统插件在用户断开连接或空闲 30 分钟后退出。退出前先调用 `ContentPluginUnloading`、`FsDisconnect`（如果插件导出了这些函数）。
- 主进程用 **Job Object** 管理所有桥接进程，设置 `JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE`：主进程退出时桥接进程一起退出，不会残留。
- 位数（Q3：只支持 64 位）：`.wfx64 / .wlx64 / .wdx64` 直接交给 x64 桥接。老扩展名 `.wfx / .wlx / .wdx` 先读 PE 头里的 Machine 字段：`AMD64` 照常加载（有些作者给 64 位文件也用了老扩展名）；`I386` **拒绝加载**，设置页显示“这是 32 位插件，LumaFiles 只支持 64 位插件，请向作者索取 64 位版本”；ARM64 等其他架构显示“不支持的架构”。读 PE 头只解析文件字节，**不执行** DLL。
- 崩溃处理：桥接进程意外退出时，所有正在进行的调用返回 `PluginError::Crashed`，`state` 变为 `Crashed { count }`。下次用到时自动重启，**10 分钟内崩溃 3 次就自动禁用**，设置页显示原因和“重新启用”按钮。

### 5.2 管道协议（`nohrs-tcbridge-proto`）
- 管道名：`\\.\pipe\lumafiles-plugin-<主进程pid>-<插件id>-<随机64位>`。由主进程创建，安全描述符只允许当前用户 SID。管道名和一次性令牌通过命令行传给桥接进程，桥接进程连接后第一帧必须回送令牌，不匹配就断开。
- 帧格式：`u32 总长度 | u8 帧类型 | u64 请求id | u32 json长度 | json | 二进制负载`。二进制负载只用于图片（BGRA）和文件头部字节，**禁止**把字节数组塞进 JSON。
- 帧类型：`Request`（主→桥）、`Response`（桥→主）、`Callback`（桥→主，插件回调）、`CallbackReply`（主→桥）、`Event`（桥→主，单向通知，如日志、查看器窗口发来的按键）。
- 并发：主进程可以连续发出多个请求；桥接进程内部按规则串行执行（§5.4），按请求 id 回应，回应可以乱序。
- 版本：握手时双方交换 `proto_version`，不一致就拒绝连接并在设置页提示“桥接程序版本不匹配，请重新安装”。
- 体积上限：单帧最大 64 MB；目录列表超过 50 000 项时分批返回（`ListChunk`），每批 5 000 项。

### 5.3 函数映射与分期

| TC 导出函数 | 宿主方法 | 阶段 | 备注 |
|---|---|---|---|
| **WDX** `ContentGetDetectString` / `ContentSetDefaultParams` / `ContentGetSupportedField` / `ContentGetValue(W)` | `detect` / 初始化 / `fields` / `value` | P1 | 优先调用 W 版本，没有才用 ANSI 版本 |
| `ContentStopGetValue(W)` / `ContentGetDefaultSortOrder` / `ContentPluginUnloading` / `ContentGetSupportedFieldFlags` / `ContentSendStateInformation(W)` | 同名方法 | P1 | |
| `ContentSetValue(W)` / `ContentEditValue` | `set_value` / 编辑对话框 | P1.5 | 和批量改属性一起做 |
| `ContentCompareFiles` / `ContentFindValue` / `ContentGetSupportedOperators` | 比较、搜索 | P5 | 2.12 新增；P1 搜索时由宿主自己比较 |
| **WLX** `ListLoad(W)` / `ListLoadNext(W)` / `ListCloseWindow` / `ListGetDetectString` / `ListSetDefaultParams` | `open` / `load_next` / 关闭 / `detect` / 初始化 | P2 | |
| `ListSendCommand` / `ListSearchText(W)` / `ListNotificationReceived` | 复制、全选、换行与适应窗口 / `search` / 消息转发 | P2 | |
| `ListGetPreviewBitmap(W)` | `thumbnail` | P2 | 返回 HBITMAP，由桥接进程转成 BGRA 再传回 |
| `ListPrint(W)` / `ListSearchDialog` | 打印 / 插件自带的搜索对话框 | P2.5 | |
| **WFX** `FsInit(W)` / `FsSetDefaultParams` / `FsGetDefRootName` / `FsFindFirst(W)` / `FsFindNext(W)` / `FsFindClose` | 初始化 / `root_name` / `list` | P3 | `FsFindFirst` 返回 `INVALID_HANDLE_VALUE` 且错误码为 `ERROR_NO_MORE_FILES` 时表示空目录，不是失败 |
| `FsGetFile(W)` / `FsPutFile(W)` / `FsRenMovFile(W)` / `FsDeleteFile(W)` / `FsMkDir(W)` / `FsRemoveDir(W)` | 传输与文件操作 | P3 | |
| `FsExecuteFile(W)` / `FsSetAttr(W)` / `FsSetTime(W)` / `FsDisconnect(W)` / `FsStatusInfo(W)` | 同名方法 | P3 | |
| `FsSetCryptCallback(W)` | 凭据，见 §8.5 | P3 | |
| `FsExtractCustomIcon(W)` / `FsGetPreviewBitmap(W)` | `icon` / `preview` | P3 | |
| `FsContent*` | `content()` | P3 | 复用内容插件的整套接入 |
| `FsGetBackgroundFlags` | 后台传输 | P3.5 | |
| `FsLinksToLocalFiles` / `FsGetLocalName(W)` | 临时面板类插件 | P5 | 有了它们才能对插件里的文件直接用本地操作 |

- **常量和结构体布局一律以官方头文件为准**（`fsplugin.h`、`listplug.h`、`contplug.h`），在 `nohrs-tcbridge/src/sdk.rs` 中翻译成 Rust，每个常量注释出处。**禁止**凭记忆手写数值。
- 某个函数插件没有导出，就把对应能力位置为 0；界面上对应入口置灰，**不要**调用后再处理报错。

### 5.4 线程与超时（桥接进程内）

| 线程 | 跑什么 | 规则 |
|---|---|---|
| UI 线程 | `ListLoad*`、`ListCloseWindow`、`ListSendCommand`、`ListSearchText`、所有窗口消息 | 只要有查看器会话就必须持续处理消息，**绝不能**被其他插件调用阻塞 |
| 调用线程 | 其余所有 WFX 和 WDX 调用 | **严格串行**。TC 插件默认不是线程安全的 |
| 后台传输线程 | 仅 `FsGetBackgroundFlags` 声明支持的 `FsGetFile` / `FsPutFile` | 同时最多 1 个后台传输，与调用线程并行 |

超时（由主进程计时，超时后不再等待；桥接进程继续执行，结果丢弃）：

| 调用 | 超时 | 超时后 |
|---|---|---|
| `ContentGetValue`（快速取值，`allow_slow=false`） | 500 ms | 按 `Delayed` 处理，转入慢速队列 |
| `ContentGetValue`（慢速取值） | 30 s | 显示“—”，同一会话内这个文件的这个字段不再取值 |
| `ListGetPreviewBitmap` | 5 s | 用 Shell 缩略图 |
| `FsFindFirst/Next` | 不超时，可取消 | 用户取消后放弃结果；10 s 仍没返回则杀掉桥接进程，因为 WFX 列目录没有取消接口 |
| 其他 WFX 调用 | 不超时 | 由进度回调返回“中止”来取消 |
| 任意调用期间桥接进程 60 s 没有任何回应（心跳也没有） | — | 判定卡死，杀掉进程并按崩溃处理 |

### 5.5 回调映射

| TC 回调 | 宿主行为 |
|---|---|
| `ProgressProc(PluginNr, Source, Target, Percent)` | 转成 `Progress::report`，进入现有的传输队列界面。返回值 1 表示用户点了取消 |
| `LogProc(PluginNr, MsgType, Text)` | 写入 `tracing`（target `plugin.<id>`）。`MSGTYPE_CONNECT` 时在状态栏显示“已连接 …”；`IMPORTANTERROR` 时弹出提示（不是模态框） |
| `RequestProc(PluginNr, Type, Title, Text, ReturnedText, MaxLen)` | 弹出对应的对话框：用户名、密码（掩码显示）、目标目录、URL、确认。桥接线程等待用户回答，**没有超时**；用户取消则回调返回 FALSE |
| `CryptProc(PluginNr, CryptoNr, Mode, ConnectionName, Password, MaxLen)` | 见 §8.5 |

### 5.6 初始化参数与 ini 文件
- `*SetDefaultParams` 结构里的 `DefaultIniName` 固定传 `{tc_ini_dir}\wincmd.ini`，其中 `tc_ini_dir = {config}\plugins\tc\`。**不要**创建这个 `wincmd.ini`（插件只取它所在的目录，用来放自己的 ini）。如果有插件确实要读写 `wincmd.ini`，就让它读写这个隔离文件，绝不碰用户真实的 TC 配置。
- 版本号字段按对齐基准填写：WFX 2.1、WLX 2.13、WDX 2.12。
- 环境变量：启动桥接进程时设置 `COMMANDER_PATH = {tc_ini_dir}` 和 `COMMANDER_INI = {tc_ini_dir}\wincmd.ini`，因为不少插件靠这两个变量找配置目录。**需要核实**：TC 实际设置的是哪几个变量；以真实插件的行为为准，未决时记入 PR 的“未决问题”。

### 5.7 编码
- 只调用 W（UTF-16）函数。只导出 ANSI 版本的插件**拒绝加载**，设置页显示原因（Q4 随 Q3 一起确定：64 位插件按规定都提供 W 版本，不做 ANSI 兼容）。
- 路径长度：W 版本函数使用 `MAX_PATH` 以上的缓冲区，具体长度以头文件为准。

### 5.8 已知不兼容（必须写进 SDK 文档 §11）
- 插件通过窗口类名查找 TC 主窗口（`TTOTAL_CMD`）并向它发消息的功能**不可用**。
- 查看器插件向父窗口发送 TC Lister 的私有消息时，只支持 SDK 文档化的 `WM_COMMAND` 通知；其他消息忽略。
- 依赖 TC 内置命令（`cm_*`）的功能不可用。
- 插件弹出的模态对话框：父窗口是桥接进程里的窗口（查看器）或桌面窗口加 owner（WFX），可能不在 LumaFiles 窗口中央。

---

## 6. 配置：`plugins.toml`

位置：`paths::plugins_file() = active_config_dir().join("plugins.toml")`。插件文件默认安装到 `paths::plugins_dir() = data_dir().join("plugins")`，也允许引用任意绝对路径（例如用户现有的 TC 插件目录）。便携模式下路径可以写 `{portable}\...`，展开规则复用快速启动的 `launch::expand`。

```toml
schema_version = 1

[bridge]
per_plugin_process = true     # false = 同位数的 TC 插件共用一个桥接进程（省内存，隔离变差）
viewer_idle_minutes = 10
fs_idle_minutes = 30

[viewer_policy]
order = "plugins_first"       # plugins_first（默认，Q2 已确认）| builtin_first
quick_view = true             # 预览面板里是否使用查看器插件
thumbnails = true             # 是否用 ListGetPreviewBitmap 补充缩略图

[[fs]]
id = "sftp"
path = '{portable}\plugins\sftpplug\sftpplug.wfx64'
root_name = ""                # 留空 = 取 FsGetDefRootName
enabled = true
sha256 = "…"                  # 首次信任时记录；文件变化后要重新确认（§13）

[[viewer]]                    # 数组顺序就是优先级
id = "imagine"
path = 'D:\TC\plugins\wlx\Imagine\Imagine.wlx64'
detect = 'EXT="JPG" | EXT="PNG" | EXT="WEBP"'   # 留空 = 取插件自带检测字符串
use_in = ["viewer", "quick_view", "thumbnail"]
enabled = true
sha256 = "…"

[[content]]
id = "mediainfo"
path = '…\mediainfo.wdx64'
detect = ""
enabled = true
sha256 = "…"

[[content]]
id = "linecount"
package = "linecount"         # WASM 原生插件：引用 plugins_dir 下的包名，不写 path
enabled = true
```

- 模型和校验写在 `nohrs-plugins/src/config.rs`，规则参照 `quicklaunch.toml`：`deny_unknown_fields`、id 只允许 `[a-z0-9_]{1,32}`、数组中 id 重复时保留第一个并报警告、`schema_version` 比当前版本新时只读不写、写回时原子写入并保留 `.bak`、热重载复用 root 的 400 ms 循环。
- **id 跨三个数组唯一**。同一个 DLL 既是 WFX 又是 WDX 时，在 `[[fs]]` 和 `[[content]]` 里各写一条，用 `path` 相同来识别同一个 DLL（见 §5.1）。

### 6.1 从 Total Commander 导入（P1 起，每个阶段导入对应的那一类）
- 设置页提供“从 Total Commander 导入…”：自动查找 `%APPDATA%\GHISLER\wincmd.ini`，也可以手动选择 `wincmd.ini`。
- 读取 `[FileSystemPlugins]`、`[ListerPlugins]`、`[ContentPlugins]` 三节（以及 64 位 TC 可能用到的对应节），解析插件路径、检测字符串和顺序。**节名和键名的具体格式以真实 wincmd.ini 为准**，实现前请用 BoBO 机器上的 TC 配置样本核实。
- 路径中的 `%COMMANDER_PATH%` 展开为 TC 安装目录：先看 ini 里的 `InstallDir` 或注册表，找不到就让用户选择。
- 导入只**添加**插件，不删除也不覆盖已有的同名条目。导入前先列出预览清单，让用户勾选。

### 6.2 安装插件包
- 用户把 TC 插件 zip 包拖进 LumaFiles，或者在设置页点“添加…”选中 zip：读取包里的 `pluginst.inf`（TC 自动安装格式：`[plugininstall]` 节中的 `type`、`file`、`description`、`defaultdir`），解压到 `plugins_dir\<defaultdir>`，生成对应条目。**需要核实**：`pluginst.inf` 的完整键集合以 TC 帮助为准。
- 用户直接选择 `.wfx64 / .wlx64 / .wdx64` 文件：不复制，原地引用。
- WASM 原生插件包（`.lfp`，即 zip）：见 SDK 文档 §10。

---

## 7. 内容插件接入（P1）

### 7.1 列集（新功能，属于 P1 范围）
- 详细信息视图支持**列集**：一组列定义，可以按目录或按文件类型自动切换，相当于 TC 的“自定义列视图”。
- 列集单独存放在 `columns.toml`，**不写进** `config.toml`：

```toml
schema_version = 1
[[set]]
id = "media"
name = "媒体"
auto = 'EXT="MP4" | EXT="MKV" | EXT="MP3"'   # 当前目录里大部分文件命中这个检测字符串时自动切换；留空 = 只能手动切换
columns = [
  { source = "builtin:name" },
  { source = "builtin:size" },
  { source = "plugin:mediainfo/Duration", unit = "", width = 80, align = "right" },
  { source = "plugin:mediainfo/Width", width = 60, align = "right" },
]
```

- `source` 中的字段按**名字**引用，不按编号引用：插件升级后字段顺序可能变化。加载时把名字解析成编号；找不到的字段，该列显示“?”，鼠标悬停提示原因。
- 列标题右键菜单增加“添加插件列 ▸ 插件 ▸ 字段 ▸ 单位”。

### 7.2 取值流程
1. 进入目录时调用 `state(ReadNewDir)`；按 F5 刷新时调用 `state(RefreshPressed)`。
2. **只对可见行取值**。滚动时按行请求，请求进入**该插件**的队列，不同插件的队列互相独立。
3. 第一轮用 `allow_slow=false` 取值。返回 `Delayed` 的格子先显示“…”，放进慢速队列，第二轮用 `allow_slow=true`。
4. 返回 `OnDemand` 的字段（插件标记为“只在明确需要时计算”）：列里显示“按空格计算”，选中后按空格才取值，与 TC 的行为一致。
5. 离开目录时，对还没完成的慢速请求调用 `stop(file)`，并清空队列。
6. 缓存键为 `(插件id, 字段, 单位, 路径, 修改时间, 大小)`，内存 LRU 最多 50 000 项。文件变化（监视器通知）后，该文件的缓存失效。
7. 排序：按插件列排序时，只有在所有行都已取到值之后才能排序；没取到的值先排到最后，并在状态栏显示“正在计算 n 项…”。

### 7.3 批量重命名
- 占位符与 TC 对齐：`[=插件id.字段名]`、`[=插件id.字段名.单位]`。例：`[=mediainfo.Duration]`。（现有规则里已经有 `[N]`、`[E]`，在同一个解析器里扩展。）
- 预览列表里的取值一律使用 `allow_slow=true`，在后台执行并显示进度；用户可以取消。

### 7.4 搜索
- 搜索窗口新增“插件”页：添加任意条件“插件 / 字段 / 运算符 / 值”，条件之间用“且 / 或”连接。
- P1 只做宿主侧比较：数值、日期支持 `= != < > <= >=`；文本支持“包含、不包含、等于、正则”；布尔、选项支持 `= !=`。
- 插件条件只作用在名称搜索的结果上（后过滤），在后台执行，显示进度。

### 7.5 修改字段值（P1.5）
- 只对带“可编辑”标志的字段开放。
- 入口：右键菜单“修改属性 ▸ 插件字段”，可以批量修改。多文件依次调用，首尾分别带上 `SetBatch::First` / `SetBatch::Last`（对应 SDK 的 setflags）。

---

## 8. 文件系统插件接入（P3）

### 8.1 路径与导航
- 资源管理器的 `cwd` 仍然是字符串。约定以 `\\\` 开头的就是插件路径：`\\\<根名>\<插件内路径>`，与 TC 的网上邻居写法一致。
- `\\\` 本身是插件根目录，列出所有启用的文件系统插件，每个显示为一个文件夹，图标来自插件，没有就用默认图标。
- 侧边栏新增“插件”分组，列出所有文件系统插件的根。
- 地址栏直接输入 `\\\Secure FTP\server1\dir` 即可跳转；也接受 `plugin://sftp/server1/dir`，自动换算成前一种写法。
- 根名冲突时（两个插件的根名相同），后面那个自动加后缀 ` (2)`。

### 8.2 插件目录中不可用的功能（P3）
以下入口在插件路径下必须置灰或隐藏，**不允许**拿插件路径去调用本地文件系统 API：标签、空间地图、Everything 搜索、文件夹监视（改为手动刷新，或在操作完成后由 `FsStatusInfo` 的结束通知触发刷新）、“在此打开终端”、属性对话框（改为调用 `Verb::Properties`）、拖出到 Windows 资源管理器（P5 再做：先下载到临时目录再拖出）。

### 8.3 文件操作
在 `nohrs-services/src/vfs.rs` 里实现，统一接入现有的传输队列和冲突处理界面：

| 源 → 目标 | 实现 |
|---|---|
| 本地 → 插件 | `put_file`，按需带覆盖、续传标志 |
| 插件 → 本地 | `get_file` |
| 插件 A → 同一个插件 | `transfer_within(mv)` |
| 插件 A → 插件 B | 先 `get_file` 到 `{cache}\plugins\transfer\`，再 `put_file`，完成后删除临时文件 |
| 删除 | 文件用 `delete`，目录用递归 `list` 加 `remove_dir`。**插件目录没有回收站**，必须二次确认 |
| 新建文件夹、重命名 | `make_dir`、`transfer_within(mv=true)` |

- 每批操作开始和结束时调用 `status(Start/End, op)`，对应 `FsStatusInfo`。
- 冲突：插件返回“已存在”或“已存在且可续传”时，弹出现有的冲突对话框；用户选“续传”时带上续传标志重新调用。

### 8.4 打开文件
`execute(Verb::Open)` 的结果：
- `Done`：插件自己处理完了。
- `Navigate(path)`：跳转到插件返回的路径（可能是本地路径，也可能是另一个插件路径）。
- `OpenLocally`：下载到 `{cache}\plugins\open\<插件id>\<随机目录>\<文件名>`，用默认程序打开，并**监视这个文件**。文件被修改后，在状态栏提示“是否上传回 …？”，确认后执行 `put_file` 覆盖。临时目录在 LumaFiles 退出时清理，24 小时以上的残留在下次启动时清理。

### 8.5 凭据
- `FsSetCryptCallback` 传入的选项中声明“主密码已设置”（对应头文件中的常量），让插件使用宿主提供的密码存储。
- `CryptProc` 落到 **Windows 凭据管理器**（`CredWriteW` / `CredReadW` / `CredDeleteW`），目标名为 `LumaFiles/plugin/<插件id>/<连接名>`。复制、移动对应重命名操作；不弹界面的读取请求只读、不提示。这部分代码放在主进程，桥接进程只负责转发。
- **不**实现 TC 的主密码机制，也**不**读取 TC 的 `wcx_ftp.ini` 等加密存储。

### 8.6 后台传输（P3.5）
插件声明支持后台下载或上传时，传输进入现有的后台队列，在桥接进程的后台线程上执行（§5.4）；不支持时，传输期间该插件的其他调用排队等待，界面上显示“插件忙”。

---

## 9. 查看器插件接入（P2）

### 9.1 查看器窗口（F3）：由桥接进程拥有
- 原生 TC 插件需要一个父 HWND。P2 把**整个查看器窗口放在桥接进程里**：一个普通的 Win32 框架窗口（标题栏加菜单），插件窗口是它的子窗口。这样避免了跨进程的父子窗口，是最稳的做法。
- 窗口的 owner 设为 LumaFiles 主窗口（允许跨进程设置 owner），这样它跟随主窗口最小化，并显示在主窗口之上。
- 菜单和按键与 TC Lister 对齐（只做列出的这些）：
  - `1`–`7` 切换模式：`1` 文本、`2` 二进制、`3` 十六进制、`4` 插件或多媒体、`5` HTML、`6` Unicode、`7` UTF-8。P2 只做 `1` 和 `4`，其他按键切到 LumaFiles 内置文本查看。
  - `N` / `P` 切换到下一个、上一个插件；`Ctrl+F` / `F3` 搜索；`Ctrl+C` 复制；`Ctrl+A` 全选；`W` 自动换行；`F` 适应窗口；`Esc` 关闭；`空格` / `Backspace` 查看下一个、上一个文件（先尝试 `load_next`，不行就重新 `open`）。
- 插件把按键转发给父窗口时（TC 约定插件把自己不处理的按键发回 Lister），框架窗口按上面的映射处理。
- 深色模式：LumaFiles 当前是深色主题时，`ViewOptions.dark = true`，映射为 2.13 的深色模式标志；框架窗口用 `DwmSetWindowAttribute(DWMWA_USE_IMMERSIVE_DARK_MODE)`。
- 用户按 `N` 一路切到最后，或者没有任何插件匹配时：关闭插件窗口，用 LumaFiles 的内置预览打开这个文件（内置预览是 gpui 窗口，和插件窗口相互独立）。

### 9.2 预览面板（Ctrl+Q 的对应物）：需要先做技术验证
目标：在 gpui 主窗口的预览面板区域里嵌入插件窗口。做法：
1. `nohrs-pages/src/plugin_slot.rs` 在 gpui 主窗口的 HWND 下创建一个 `WS_CHILD` 的“插槽”窗口，每帧布局变化后用 `SetWindowPos` 让它对齐预览面板的位置和大小。
2. 把插槽窗口的 HWND 交给桥接进程，作为 `ListLoadW` 的 `ParentWin`。这就成了**跨进程的父子窗口**。

跨进程父子窗口会让两个线程的输入队列连在一起：任何一方卡住，另一方的输入也会卡住。因此 **P2 第一步必须做技术验证（Spike S1）**，逐项验证并记录结论：
- a. 插件子窗口能画在 gpui 的 DirectX 内容**之上**，没有闪烁和黑块。
- b. 预览面板拖动大小、切换左右位置、窗口最大化时，插件窗口能跟上。
- c. 焦点：点击插件窗口能接收键盘输入，`Tab` / `Esc` 能回到 LumaFiles。
- d. **卡死隔离**：用一个故意在 `ListLoadW` 里无限循环的测试插件，验证 LumaFiles 主窗口还能操作；如果被拖住，看 3 秒后杀掉桥接进程能否恢复。
- e. 用基于 WebView2 的插件（例如 TorrentLister_WLX）和 Chromium 内核的插件（例如 wlx-edge-viewer）各测一次。

Spike 任何一项失败：预览面板**不嵌入窗口**，改为显示 `thumbnail()` 的静态图，加一个“在查看器中打开（F3）”按钮。把失败原因记录在 `docs/PERFORMANCE.md` 或新的 ADR 里，留给以后处理。

### 9.3 缩略图
`thumbnails.rs` 的取图顺序变为：自定义缩略图（如果有）→ Shell → **查看器插件的 `thumbnail()`**（按插件优先级，只问 `use_in` 里包含 `thumbnail` 的插件）→ 类型图标。插件缩略图使用同一套磁盘缓存，缓存键里加上插件 id 和插件文件的修改时间。

### 9.4 与内置预览的顺序（Q2 已确认：一律插件优先）
- 默认 `viewer_policy.order = "plugins_first"`：按 `[[viewer]]` 的数组顺序依次尝试，第一个检测字符串匹配并且 `open` 成功的胜出；**所有插件都不匹配或都打开失败**时，才用 LumaFiles 内置预览。图片、PDF、音视频、3D 文件**不做例外**，与 TC 行为一致。
- 同样适用于预览面板（`quick_view = true` 时）和缩略图以外的所有查看场景；缩略图的顺序仍按 §9.3（Shell 优先，因为它快且有系统缓存）。
- 用户可以把 `order` 改成 `builtin_first`。另外在查看器窗口里按 `4` 回到插件、按 `N` / `P` 切换插件；内置预览中提供“用插件打开”入口，方便临时切换。
- 性能约束：插件优先意味着每打开一个文件都要先求值所有查看器插件的检测字符串。求值必须在后台进行；用到 `FIND` 时，所有插件共享同一份已读取的文件头（§4.2）。预览面板切换文件时，**等待插件结果最多 300 ms**，超过就先显示内置预览，等插件加载完成后再替换，避免方向键快速浏览时出现空白。

---

## 10. WASM 原生插件（P4）

- 三个 world（`fs-plugin` / `viewer-plugin` / `content-plugin`）和所有类型定义在 SDK 文档 §3 中定稿，WIT 文件放在 `crates/nohrs-plugin-host/wit/`，SDK 模板仓库从这里复制。
- 与现有 `docs/plugin-api.md`（`nohrs:plugin@0.1.0`）的关系：
  - `previews` 由 `viewer` 取代；`decorations`（图标角标、颜色）保留；`commands`、`events` 保留。
  - 新版本为 `lumafiles:plugin@0.2.0`（包名见 §17 Q1）。0.1.0 还没有实现，也没有用户，直接废弃，不做兼容。
  - 两条路线和本文的 D1–D7 决策已记录在 [ADR 0010](./adr/0010-tc-plugin-kinds-and-native-bridge.md)；`plugin-api.md`、`plugin-overview.md`、`ROADMAP.md` 已加上指向本文的说明。
- **权限**：沿用 `docs/plugin-permissions.md`，新增两项：
  - `sockets = ["host:port", "*.example.com:22"]`：给 SFTP 这类需要原始 TCP 的文件系统插件用，走 WASI sockets；
  - `edit_fields = true`：内容插件需要修改文件元数据时声明。
- **按句柄授权**：查看器插件和内容插件**不需要** `read_paths` 权限。宿主只把用户正在看的文件以只读 `blob` 句柄传给插件，插件拿不到路径之外的任何文件。

---

## 11. 跨平台
- 整套 TC 兼容层只支持 Windows：所有代码放在 `#[cfg(windows)]` 下，其他平台的设置页不显示“添加 TC 插件”。
- WASM 原生插件全平台可用；它们的 `Surface` 一律是 `Rendered`。

---

## 12. 设置页“插件”（对应截图）

`SettingsSection` 新增 `Plugins`，排在 `Integrations` 之后，内容放在 `settings/plugins.rs`。

```text
插件
├─ 文件系统插件（WFX）  用来访问特殊的磁盘分区、外部设备或网络服务。        [配置]
├─ 查看器插件（WLX）    用来查看图像、电子表格、数据库等特殊格式的文件。    [配置]
├─ 内容插件（WDX）      用来显示文件信息，用于自定义列、文件搜索和重命名。  [配置]
└─ [从 Total Commander 导入…]   [安装插件包…]
```

点“配置”后进入该类插件的列表页：

| 列 | 说明 |
|---|---|
| 启用 | 复选框 |
| 名称 | 显示名；TC 插件取文件名，WASM 插件取 `plugin.toml` 里的 name |
| 类型 | `TC` / `原生` |
| 版本 | 取 DLL 的版本资源或 `plugin.toml` |
| 检测字符串 / 根名 | 可以编辑，解析失败时标红并显示原因 |
| 状态 | 正常 / 未启动 / 已崩溃 n 次 / 已禁用（原因） / 文件缺失 |

按钮：添加…、移除、上移、下移（调整优先级）、**插件设置**（见下文）、打开插件目录、查看日志。

“插件设置”按钮的行为：
- TC 文件系统插件：对插件根 `\` 调用 `FsExecuteFile` 的 `properties` 动作（TC 用户在网上邻居里对插件按 Alt+Enter 也是这个效果）。
- TC 查看器、内容插件：没有统一的设置入口。在 `tc_ini_dir` 下找 `<dll 文件名>.ini`，找到就用默认编辑器打开，找不到则按钮置灰并提示“该插件没有提供设置”。
- WASM 插件：如果插件导出了 `configure`，按它返回的表单描述绘制设置对话框。

---

## 13. 安全
- TC 原生插件是**完全信任的本机代码**，没有沙箱。首次启用时必须弹出提示：“此插件以原生代码运行，拥有与你相同的权限。只安装你信任来源的插件。”用户确认后，把文件的 SHA-256 记入 `plugins.toml`。**文件哈希变化后**（插件被更新或替换），下次加载前要求重新确认（提示强度见 §17 Q6）。
- 桥接进程与主进程同一权限运行，**不**提权。插件需要管理员权限时自己处理（大多数插件不需要）。
- 管道的访问控制和令牌按 §5.2 执行，防止其他进程冒充桥接进程注入回应。
- WASM 插件按 `docs/plugin-permissions.md` 执行。

---

## 14. 性能预算（release 构建，PR 中贴出中位数）

| 指标 | 门槛 |
|---|---|
| 桥接进程冷启动到握手完成 | ≤ 150 ms |
| 单次内容字段取值的 IPC 额外开销（插件本身耗时不计） | ≤ 0.5 ms |
| 1 000 行 × 3 个插件列，首屏可见的 40 行全部出值（插件本身很快的情况） | ≤ 100 ms |
| 插件文件系统列出 10 000 项的 IPC 额外开销 | ≤ 80 ms |
| 任意插件卡死时，LumaFiles 主窗口仍然可以操作 | 必须满足 |
| 前台线程上任何一次插件相关调用 | ≤ 4 ms（只允许入队和读缓存） |

---

## 15. 里程碑

| 阶段 | 内容 | 验收要点 |
|---|---|---|
| **P0 技术验证** | 桥接进程骨架、管道协议、握手、Job Object、崩溃后自动重启；加载一个真实的 64 位 WDX 并在命令行打印字段（新增 `noh plugin probe <dll>`）；Spike S1（§9.2） | probe 能打印字段和值；杀掉桥接进程后自动重启；S1 的 a–e 每项都有结论 |
| **P1 内容插件** | `nohrs-plugins` 的配置、注册表、检测字符串；TC 内容插件全套接入；列集；批量重命名占位符；搜索“插件”页；设置页的内容插件部分；从 TC 导入内容插件 | 至少 3 个真实 64 位 WDX 在列、重命名、搜索里都能用；延迟字段、按需字段的行为与 TC 一致 |
| **P1.5** | 修改字段值 | |
| **P2 查看器插件** | 查看器窗口（§9.1）、缩略图、设置页的查看器部分、导入查看器插件；预览面板嵌入（取决于 S1 结论） | 至少 3 个真实 WLX（含一个 WebView2 类插件）；`N` / `P` / 搜索 / 复制可用；插件卡死不影响主窗口 |
| **P3 文件系统插件** | 插件路径、侧边栏入口、浏览、复制进出、删除、新建、重命名、打开（三种结果）、凭据、插件列 | 至少 2 个真实 WFX（一个本地类，例如插件管理器；一个网络类，例如 SFTP） |
| **P3.5** | 后台传输、自定义图标、预览图 | |
| **P4 WASM 原生插件** | `nohrs-plugin-host` 加三个 world、Rust 模板、三个示例插件、`noh plugin new/build/dev` | 三个示例插件分别能作为列、查看器、文件系统使用 |
| **P5** | TS / Python 模板；临时面板类插件；拖出到 Windows 资源管理器；2.12 的比较和搜索运算符；`.lng` 字段名翻译 | |
| **P6 压缩包插件（WCX）** | Q5 确认列入路线图，**本期不做**。届时另写设计文档：对齐 TC 压缩包插件接口；与内置的 zip 浏览（`explorer/archive.rs`）合并为统一的“压缩包提供者”；同样放在桥接进程里运行，只支持 64 位 | 本文的各个 trait 不为 WCX 预留任何接口，避免空接口 |

**兼容性测试清单**（每个阶段选用其中对应类型的插件，结果写进 [`docs/plugin-compat.md`](./plugin-compat.md)，表格已建好）：
- 文件系统：tc-plugins（插件管理器，用它的 64 位版本，<https://github.com/naderi/tc-plugins>）。
- 查看器：AKFont（x64）；wlx-edge-viewer（用 64 位版本，Chromium 内核，<https://github.com/rg-software/wlx-edge-viewer>）；wlx-multidoc-viewer（<https://github.com/rg-software/wlx-multidoc-viewer>）；TorrentLister_WLX（WebView2，<https://github.com/mikinko/TorrentLister_WLX>）。
- 内容：SVNdetails（x64）；MSWord_WDX（<https://github.com/CoderX40/MSWord_WDX>）。
- 工具：WDX Test 64（<https://totalcmd.net/plugins/wdxtest64>），用来对照同一个 WDX 在 TC 中的取值结果。

---

## 16. 禁止事项与未决处理

- **禁止**在主进程里 `LoadLibrary` 任何第三方插件 DLL，哪怕只是为了读取版本信息（版本资源用 `GetFileVersionInfoW` 读取，它不执行 DLL 代码）。
- **禁止**在 gpui 前台线程上做任何管道读写、文件头读取或插件调用。
- **禁止**读写用户真实的 TC 配置文件（导入功能只读）。
- **禁止**凭记忆手写 TC SDK 的常量值和结构体布局（§5.3）。
- **禁止**为了兼容某个插件而在主进程中加入针对它的特殊处理；需要特殊处理的，写进桥接进程的 `quirks.rs`，并按插件文件名加 SHA-256 匹配。
- 桥接进程 crate 不得依赖 `nohrs-core` 及以上（§2.1）。
- 新增界面文案一律使用 `t!`，9 种语言补齐。
- 遇到本文没有规定的情况：选改动最小、行为最保守的做法，写进 PR 的“未决问题”，**不要**扩大改动范围。

---

## 17. 待 BoBO 确认

| # | 问题 | 结论 |
|---|---|---|
| Q1 | WASM 插件的公开包名：`lumafiles:plugin@0.2.0`，还是沿用 `nohrs:plugin` | 未确认，按默认：`lumafiles:plugin`（面向插件作者的是产品名；内部 crate 名不变） |
| Q2 | 打开文件时查看器的顺序 | ✅ **已确认：一律插件优先**，图片、PDF、音视频、3D 不例外（§9.4） |
| Q3 | 是否支持 32 位插件 | ✅ **已确认：只支持 64 位**。不构建 x86 桥接；32 位 DLL 拒绝加载并提示原因（§5.1） |
| Q4 | 是否支持只有 ANSI 函数的老插件 | ✅ 随 Q3 确定：**不支持**（§5.7） |
| Q5 | 压缩包插件（WCX） | ✅ **已确认：列为 P6**，本期不做（§15） |
| Q6 | TC 原生插件的安全提示时机 | 未确认，按默认：首次启用和哈希变化时都提示 |
| Q7 | 是否在 P1 就提供“从 Total Commander 导入” | 未确认，按默认：是。导入时遇到 32 位插件，列出来并标记“不支持（32 位）”，不导入 |
