# LumaFiles 插件 SDK 开发指南 · v0.2（草案）

> 适用于：LumaFiles 插件 API `lumafiles:plugin@0.2.0`，以及 Total Commander 插件兼容层（文件系统 2.1 SE、查看器 2.13、内容 2.12）。
> 状态：接口已定稿，等待实现。TC 兼容层按 P1（内容）→ P2（查看器）→ P3（文件系统）的顺序上线，WASM 原生插件在 P4 上线。各章节标明了对应阶段。
> 宿主侧的实现规格见 `docs/LumaPlugin-设计与边界约定.md`。

---

## 0. 先确定你走哪条路

| 你的情况 | 走哪条路 | 看哪几章 |
|---|---|---|
| 已经有 **64 位** Total Commander 插件（.wfx64 / .wlx64 / .wdx64） | **什么都不用改**，LumaFiles 直接加载（不支持 32 位插件） | §11 |
| 要写新插件，希望跨平台、自动沙箱、一次打包到处可用 | **原生插件**（WASM 组件，Rust / TypeScript / Python） | §1–§10 |
| 要写新插件，必须调用 Win32 API、使用现成的 C/C++ 库，或者要嵌入原生窗口 | 按 **TC 官方 SDK** 写 TC 插件，LumaFiles 和 TC 都能用 | §11，以及 TC 官方 SDK |

三类插件与 TC 一一对应：

| 类型 | 做什么 | 出现在 LumaFiles 的哪里 |
|---|---|---|
| **文件系统插件**（对应 WFX） | 把设备、网络服务、数据库等变成一个可以浏览的目录树 | 侧边栏“插件”分组；地址栏输入 `\\\<根名>\...` |
| **查看器插件**（对应 WLX） | 显示特殊格式的文件 | F3 查看器窗口、预览面板、缩略图 |
| **内容插件**（对应 WDX） | 为文件提供额外字段（时长、分辨率、EXIF、Git 状态等） | 详细信息视图的列、批量重命名的 `[=插件.字段]`、搜索条件 |

一个插件可以同时属于多种类型，例如一个文件系统插件同时为自己的文件提供列。

---

## 1. 五分钟上手：用 Rust 写一个内容插件（P4 起可用）

目标：为文本文件提供“行数”一列。

### 1.1 准备

```bash
rustup target add wasm32-wasip2
cargo install noh-cli          # 提供 noh plugin 子命令（随 LumaFiles 一起发布）
noh plugin new linecount --kind content --lang rust
cd linecount
```

生成的目录：

```text
linecount/
├── plugin.toml          # 清单（§2）
├── Cargo.toml
├── wit/                 # 从 SDK 复制的 WIT，不要手改
│   └── deps/lumafiles-plugin/…
├── src/lib.rs
├── i18n/zh.toml         # 字段显示名的翻译（可选）
└── samples/             # 测试样本文件
```

### 1.2 代码

```rust
// src/lib.rs
wit_bindgen::generate!({ path: "wit", world: "lumafiles:plugin/content-plugin" });

use exports::lumafiles::plugin::content::{self, FieldDef, FieldFlags, FieldKind, FieldValue, GetResult};
use exports::lumafiles::plugin::lifecycle::{self, PluginConfig};
use lumafiles::plugin::types::{Blob, PluginError};

struct LineCount;

impl lifecycle::Guest for LineCount {
    fn init(_config: PluginConfig) -> Result<(), String> { Ok(()) }
    fn shutdown() {}
    fn settings_schema() -> Option<String> { None }
    fn apply_settings(_json: String) -> Result<(), String> { Ok(()) }
}

impl content::Guest for LineCount {
    fn detect_string() -> String {
        r#"EXT="TXT" | EXT="MD" | EXT="CSV" | EXT="LOG""#.into()
    }

    fn fields() -> Vec<FieldDef> {
        vec![FieldDef {
            name: "Lines".into(),             // 稳定的字段名：列定义、重命名占位符都引用它
            kind: FieldKind::Int64,
            units: vec![],
            choices: vec![],
            flags: FieldFlags::SLOW,          // 大文件可能比较慢
            sort_descending: true,
        }]
    }

    fn get(file: &Blob, field: u32, _unit: u32, allow_slow: bool) -> Result<GetResult, PluginError> {
        if field != 0 {
            return Ok(GetResult::Unsupported);
        }
        // 第一轮快速取值时，大文件先说“慢”，宿主稍后会再用 allow_slow=true 调用
        if !allow_slow && file.size() > 1024 * 1024 {
            return Ok(GetResult::Slow);
        }
        let mut lines = 0i64;
        let mut offset = 0u64;
        while offset < file.size() {
            let chunk = file.read(offset, 64 * 1024)?;
            if chunk.is_empty() {
                break;
            }
            lines += chunk.iter().filter(|byte| **byte == b'\n').count() as i64;
            offset += chunk.len() as u64;
        }
        Ok(GetResult::Value(FieldValue::Int(lines)))
    }

    fn fulltext(_file: &Blob, _field: u32, _offset: u64) -> Result<Option<String>, PluginError> { Ok(None) }
    fn set(_file: &lumafiles::plugin::types::WritableBlob, _field: u32, _unit: u32, _value: FieldValue) -> Result<(), PluginError> {
        Err(PluginError::NotSupported)
    }
    fn dir_changed(_dir: String) {}
}

export!(LineCount);
```

> 生成的绑定里，类型名和模块路径以 `wit-bindgen` 的实际输出为准；模板仓库里的代码保证能编译。

### 1.3 构建、测试、装进 LumaFiles

```bash
noh plugin build                               # 产出 target/wasm32-wasip2/release/linecount.wasm
noh plugin test --file samples/readme.md       # 在命令行打印每个字段的值，不需要打开 LumaFiles
noh plugin dev                                 # 以开发模式安装到正在运行的 LumaFiles；重新 build 后自动重载
```

然后在详细信息视图的列标题上右键 → 添加插件列 → linecount → Lines。

---

## 2. 插件清单 `plugin.toml`

```toml
[plugin]
id = "linecount"                    # [a-z0-9_]{1,32}，全局唯一
name = "行数统计"
version = "1.0.0"                   # semver
api = "0.2"                         # 目标插件 API 版本
kinds = ["content"]                 # content | viewer | fs，可以写多个
description = "为文本文件提供行数列"
authors = ["BoBO <…>"]
license = "MIT"
homepage = "https://github.com/…"
icon = "icon.png"                   # 64×64 PNG
min_lumafiles = "0.4.0"

[content]
detect = 'EXT="TXT" | EXT="MD"'     # 可选：覆盖代码里的 detect-string，方便不改代码就调整

[viewer]
detect = '…'
use_in = ["viewer", "quick_view", "thumbnail"]

[fs]
root_name = "示例云盘"

[permissions]                       # 详见 §8；不需要的权限一律不要写
network = ["api.example.com"]
sockets = ["*.example.com:22"]
edit_fields = false
```

- `kinds` 必须和组件实际导出的接口一致，否则安装时报错。
- 一个插件要同时提供多种类型时，在自己的 WIT 里组合 world（§3.6）。

---

## 3. 接口参考（WIT）

完整 WIT 随 SDK 发布在 `wit/deps/lumafiles-plugin/`。下面是定稿内容，加了注释。

### 3.1 公共类型

```wit
package lumafiles:plugin@0.2.0;

interface types {
  /// UTC 时间
  record timestamp { seconds: s64, nanoseconds: u32 }

  flags attrs { directory, readonly, hidden, system, archive, symlink, executable }

  record entry {
    name: string,                  // 只有名字，不含路径
    attrs: attrs,
    size: option<u64>,
    modified: option<timestamp>,
    created: option<timestamp>,
    accessed: option<timestamp>,
    unix-mode: option<u32>,        // 有就填，列表里会显示成 rwx
    link-target: option<string>,
  }

  /// 8 位 RGBA，未预乘 alpha，按行排列
  record image { width: u32, height: u32, rgba: list<u8> }

  variant plugin-error {
    not-found, access-denied, exists, exists-resumable, not-supported, aborted, auth-required,
    io(string), network(string), other(string),
  }

  /// 只读文件句柄。宿主只把用户正在处理的那个文件交给你，你拿不到其他文件。
  resource blob {
    name: func() -> string;        // 文件名，用来判断扩展名或显示
    size: func() -> u64;
    read: func(offset: u64, len: u32) -> result<list<u8>, plugin-error>;   // len 上限 4 MiB
  }

  /// 可写文件句柄：只有在修改字段值、且清单声明了 edit_fields = true 时才会传入
  resource writable-blob {
    name: func() -> string;
    size: func() -> u64;
    read: func(offset: u64, len: u32) -> result<list<u8>, plugin-error>;
    write: func(offset: u64, data: list<u8>) -> result<_, plugin-error>;
    truncate: func(len: u64) -> result<_, plugin-error>;
  }
}
```

### 3.2 宿主提供的能力 `host`

```wit
interface host {
  use types.{plugin-error};

  enum level { trace, debug, info, warn, error }
  log: func(level: level, message: string);

  /// 插件私有的键值存储，持久保存，每个插件互相隔离
  kv-get: func(key: string) -> option<list<u8>>;
  kv-set: func(key: string, value: list<u8>);
  kv-delete: func(key: string);

  /// 凭据：Windows 上存进凭据管理器。name 由你决定，比如连接名
  secret-get: func(name: string) -> option<string>;
  secret-set: func(name: string, value: string) -> result<_, plugin-error>;
  secret-delete: func(name: string);

  /// 向用户提问，用户取消时返回 none。只能在文件系统插件的操作过程中调用
  record prompt { title: string, message: string, default-value: string }
  variant question { text(prompt), password(prompt), confirm(prompt), choose-folder(prompt) }
  ask: func(question: question) -> option<string>;

  /// 非模态通知，有频率限制
  notify: func(message: string);

  language: func() -> string;      // "zh" / "en" / "pt" / …
  dark-mode: func() -> bool;

  /// 进度。report 返回 false 表示用户点了取消，请尽快返回 aborted
  resource progress {
    report: func(done: u64, total: option<u64>) -> bool;
    describe: func(source: string, target: string);
  }
}
```

另外每个插件都有两个预挂载的 WASI 目录：`/data`（持久保存）和 `/cache`（可能被清理）。除此之外看不到任何真实文件系统。

### 3.3 生命周期 `lifecycle`（所有插件都要导出）

```wit
interface lifecycle {
  record plugin-config {
    api-version: string,           // 宿主实现的 API 版本，例如 "0.2.1"
    language: string,
    settings: option<string>,      // 上次 apply-settings 保存的 JSON
  }
  init: func(config: plugin-config) -> result<_, string>;
  shutdown: func();

  /// 可选设置界面：返回 JSON Schema（只支持 string / number / boolean / enum），返回 none 表示没有设置
  settings-schema: func() -> option<string>;
  apply-settings: func(json: string) -> result<_, string>;
}
```

### 3.4 文件系统 `fs-provider`

```wit
interface fs-provider {
  use types.{entry, image, plugin-error, timestamp, attrs};
  use host.{progress};
  use wasi:io/streams@0.2.0.{input-stream, output-stream};

  flags capabilities {
    get, put, rename, copy-within, delete, make-dir, remove-dir,
    set-attrs, set-times, execute, resume, preview, icons,
  }

  root-name: func() -> string;
  capabilities: func() -> capabilities;

  /// 分批列目录：next 返回空列表表示结束。大目录请分批返回，界面会边收边显示
  resource listing {
    next: func(max: u32) -> result<list<entry>, plugin-error>;
  }
  list: func(path: string) -> result<listing, plugin-error>;

  /// 下载：把 remote 的内容写进 sink。offset > 0 表示续传
  get-file: func(remote: string, sink: output-stream, offset: u64, progress: borrow<progress>) -> result<_, plugin-error>;
  /// 上传
  put-file: func(remote: string, source: input-stream, size: option<u64>, overwrite: bool, offset: u64,
                 progress: borrow<progress>) -> result<_, plugin-error>;
  /// 同一个插件内的复制或移动
  transfer-within: func(from: string, to: string, move: bool, overwrite: bool) -> result<_, plugin-error>;

  delete: func(path: string) -> result<_, plugin-error>;
  make-dir: func(path: string) -> result<_, plugin-error>;
  remove-dir: func(path: string) -> result<_, plugin-error>;   // 只删空目录，递归由宿主完成
  set-attrs: func(path: string, attrs: attrs) -> result<_, plugin-error>;
  set-times: func(path: string, modified: option<timestamp>, created: option<timestamp>,
                  accessed: option<timestamp>) -> result<_, plugin-error>;

  variant verb { open, properties, chmod(string), command(string) }
  variant exec-result {
    done,                 // 你已经处理完了
    open-locally,         // 请宿主下载到临时目录，用默认程序打开（修改后会提示用户上传回去）
    navigate(string),     // 请宿主跳转到这个路径（插件路径或本地路径都可以）
  }
  execute: func(path: string, verb: verb) -> result<exec-result, plugin-error>;

  disconnect: func(root: string);
  preview: func(path: string, width: u32, height: u32) -> option<image>;
  icon: func(path: string, size: u32) -> option<image>;
}
```

### 3.5 查看器 `viewer`

```wit
interface viewer {
  use types.{blob, image, plugin-error};

  record options { wrap: bool, fit: bool, fit-larger-only: bool, center: bool, dark: bool }

  record text-doc { text: string, monospace: bool, syntax: option<string> }    // syntax 是语法高亮提示，如 "json"
  record table-doc { columns: list<string>, rows: list<list<string>>, total-rows: u64 }  // 超过 10 万行请用 rows-at 分页
  record paged-doc { page-count: u32, page-width: u32, page-height: u32 }      // 宿主按需调用 render-page

  variant document { text(text-doc), image(image), table(table-doc), paged(paged-doc) }

  flags search-flags { match-case, whole-words, backwards }
  record hit { page: u32, start: u32, len: u32 }

  resource session {
    content: func() -> document;
    rows-at: func(offset: u64, count: u32) -> list<list<string>>;              // 只对 table 有效
    render-page: func(index: u32, width: u32, height: u32) -> result<image, plugin-error>;   // 只对 paged 有效
    search: func(query: string, flags: search-flags, from: option<hit>) -> option<hit>;     // 只对 paged 有效；文本和表格由宿主搜索
    set-options: func(options: options);
  }

  detect-string: func() -> string;
  open: func(file: blob, options: options) -> result<session, plugin-error>;   // blob 归 session 所有
  thumbnail: func(file: borrow<blob>, width: u32, height: u32) -> option<image>;
}
```

文本、表格的复制、全选、搜索由 LumaFiles 完成；图片、分页文档的复制由 LumaFiles 复制当前页的图像。

### 3.6 内容 `content`

```wit
interface content {
  use types.{blob, writable-blob, plugin-error, timestamp};

  enum field-kind { int32, int64, float, date, time, datetime, boolean, choice, text, fulltext }
  flags field-flags {
    editable,        // 可以修改（需要 edit_fields 权限）
    slow,            // 可能很慢：宿主会先用 allow-slow=false 调用
    on-demand,       // 只在用户明确要求时计算（列里显示“按空格计算”）
  }
  record field-def {
    name: string,               // 稳定名称，不要随版本改动
    kind: field-kind,
    units: list<string>,        // 例如 ["秒", "分钟"]；没有单位就给空列表
    choices: list<string>,      // 只用于 choice
    flags: field-flags,
    sort-descending: bool,      // 默认是否降序
  }

  record date { year: u16, month: u8, day: u8 }
  record time { hour: u8, minute: u8, second: u8 }
  variant field-value {
    int(s64), float(f64), boolean(bool), text(string),
    date(date), time(time), datetime(timestamp), choice(u32),
  }
  variant get-result {
    value(field-value),
    empty,           // 这个文件没有这个字段
    slow,            // 现在算会很慢，请稍后用 allow-slow=true 再调用
    on-demand,       // 只有用户要求时才算
    unsupported,     // 这个文件类型不支持
  }

  detect-string: func() -> string;
  fields: func() -> list<field-def>;
  get: func(file: borrow<blob>, field: u32, unit: u32, allow-slow: bool) -> result<get-result, plugin-error>;
  /// 全文字段：从 offset 开始返回下一段文本，没有更多就返回 none
  fulltext: func(file: borrow<blob>, field: u32, offset: u64) -> result<option<string>, plugin-error>;
  set: func(file: borrow<writable-blob>, field: u32, unit: u32, value: field-value) -> result<_, plugin-error>;
  /// 用户进入了新目录或按了刷新，可以借此清掉你自己的缓存
  dir-changed: func(dir: string);
}
```

### 3.7 World

```wit
world content-plugin { import host; export lifecycle; export content; }
world viewer-plugin  { import host; export lifecycle; export viewer; }
world fs-plugin {
  import host;
  import wasi:sockets/tcp@0.2.0;                 // 需要 sockets 权限
  import wasi:http/outgoing-handler@0.2.0;       // 需要 network 权限
  export lifecycle;
  export fs-provider;
}
```

同时提供多种类型时，在你自己的 WIT 里组合：

```wit
package me:myplugin;
world plugin {
  include lumafiles:plugin/fs-plugin@0.2.0;
  include lumafiles:plugin/content-plugin@0.2.0;
}
```

---

## 4. 文件系统插件开发要点

### 4.1 路径
- 宿主传给你的路径一律是**插件内部路径**：以 `\` 开头、用 `\` 分隔，根目录就是 `\`（与 TC 一致）。不要假设路径里只有 ASCII。
- 你返回的 `entry.name` 只能是名字，不能带 `\`。

### 4.2 列目录
- 返回 `listing` 资源，宿主会反复调用 `next(max)`。网络慢的时候，先返回已经拿到的那部分，用户就能更早看到内容。
- **不要**在 `list` 里做阻塞的长时间操作而不返回；需要先连接的，在第一次 `next` 里连接。
- 需要密码时，先用 `secret-get` 读取已保存的；没有就用 `ask(password)` 问用户，问到后用 `secret-set` 保存（问一下用户要不要保存）。

### 4.3 传输与取消
- 每传完一块就调用一次 `progress.report(done, total)`；它返回 `false` 时，立刻停止并返回 `aborted`。
- 目标已存在时，如果没有 `overwrite`：能续传的返回 `exists-resumable`，否则返回 `exists`。宿主会询问用户，再用相应参数重新调用。
- 删除目录时，宿主会先删掉里面的文件，再调用 `remove-dir`。你只需要删空目录。

### 4.4 打开文件
- 双击文件时，宿主调用 `execute(path, open)`：
  - 可以直接处理的（比如“连接到服务器”这类虚拟条目），处理完返回 `done`；
  - 普通文件返回 `open-locally`，宿主会先下载再打开；
  - 快捷方式类条目返回 `navigate("\\server2\\")`。
- `properties`：用户在你的根目录或条目上按“属性”时调用，常用来打开插件的连接设置。

### 4.5 示例：把环境变量做成一个文件系统

```rust
impl fs_provider::Guest for EnvFs {
    type Listing = EnvListing;
    fn root_name() -> String { "环境变量".into() }
    fn capabilities() -> Capabilities { Capabilities::GET }
    fn list(path: String) -> Result<fs_provider::Listing, PluginError> {
        if path != "\\" { return Err(PluginError::NotFound); }
        let entries: Vec<Entry> = std::env::vars()
            .map(|(name, value)| Entry { name, attrs: Attrs::READONLY, size: Some(value.len() as u64),
                                         modified: None, created: None, accessed: None, unix_mode: None, link_target: None })
            .collect();
        Ok(fs_provider::Listing::new(EnvListing::new(entries)))
    }
    fn get_file(remote: String, sink: OutputStream, _offset: u64, progress: &Progress) -> Result<(), PluginError> {
        let name = remote.trim_start_matches('\\');
        let value = std::env::var(name).map_err(|_| PluginError::NotFound)?;
        sink.blocking_write_and_flush(value.as_bytes()).map_err(|error| PluginError::Io(error.to_string()))?;
        progress.report(value.len() as u64, Some(value.len() as u64));
        Ok(())
    }
    // 其余方法返回 Err(PluginError::NotSupported)，并且在 capabilities 里不声明对应能力
}
```

> WASI 里的环境变量是宿主传给组件的那一份，不是你电脑上的完整环境。这个例子只用来演示接口。

---

## 5. 查看器插件开发要点

- 选文档类型：
  - 纯文本、代码 → `text`，宿主负责换行、搜索、复制和高亮；
  - 一张图 → `image`；
  - 行列数据 → `table`，超过 10 万行请把 `total-rows` 设为真实行数，由宿主分页调用 `rows-at`；
  - 多页或需要按尺寸渲染的格式（PDF、PSD、CAD）→ `paged`，宿主按需调用 `render-page`，并按当前窗口尺寸和缩放传入 `width` / `height`。
- `open` 应该尽快返回。耗时的解析放到第一次 `content()` 或 `render-page()` 里，宿主会显示加载状态。LumaFiles 默认**插件优先**，你的插件会在用户每次用方向键切换文件时被调用；预览面板只等 300 ms，超时会先显示内置预览。
- 检测字符串要写准：插件优先时，匹配却打不开的文件会多出一次失败尝试。
- `thumbnail` 只读必要的部分；超过 5 秒不返回，宿主就会放弃。
- 深色模式：`options.dark` 为真时，请输出深色背景的结果（适用于 `paged` 和 `image`）。

示例（CSV 表格查看器的核心部分）：

```rust
fn open(file: Blob, _options: Options) -> Result<viewer::Session, PluginError> {
    let bytes = read_all(&file, 32 * 1024 * 1024)?;          // 自己定义的辅助函数，按块读取，最多 32 MiB
    let text = String::from_utf8_lossy(&bytes);
    let mut lines = text.lines();
    let columns = lines.next().map(split_csv).unwrap_or_default();
    let rows: Vec<Vec<String>> = lines.map(split_csv).collect();
    Ok(viewer::Session::new(CsvSession { columns, rows }))
}
```

---

## 6. 内容插件开发要点

- **字段名是公开契约**：用户的列集、批量重命名规则都按名字引用字段。改名等于破坏兼容；要改显示名，请改 `i18n/*.toml`：

  ```toml
  # i18n/zh.toml
  [fields]
  Lines = "行数"
  [units]
  ```

- **快和慢**：宿主会先用 `allow-slow=false` 调用，只为界面上可见的行取值。超过约 0.5 秒才能算出的值请返回 `slow`，宿主会排队后再用 `allow-slow=true` 调用。
- **单位**：例如 `units = ["字节", "KB", "MB"]`，`get` 里的 `unit` 就是这个列表的下标。
- **全文字段**（`fulltext`）供搜索的“包含文本”条件使用：每次返回一段文本（建议 64 KB 左右），宿主会用累计的 `offset` 继续调用。
- 字段在三个地方使用：
  - 列集：`plugin:<插件id>/<字段名>`
  - 批量重命名：`[=<插件id>.<字段名>]`，或带单位的 `[=<插件id>.<字段名>.<单位>]`
  - 搜索窗口的“插件”页：按字段类型提供运算符

字段类型与 TC 的对应关系（方便 TC 插件作者迁移）：

| LumaFiles | TC |
|---|---|
| int32 / int64 / float | ft_numeric_32 / ft_numeric_64 / ft_numeric_floating |
| date / time / datetime | ft_date / ft_time / ft_datetime |
| boolean / choice | ft_boolean / ft_multiplechoice |
| text | ft_string / ft_stringw |
| fulltext | ft_fulltext / ft_fulltextw |
| get-result：empty / slow / on-demand | ft_fieldempty / ft_delayed / ft_ondemand |

---

## 7. 检测字符串参考

检测字符串决定插件对哪些文件生效。语法与 TC 相同，宿主统一解析：

| 写法 | 含义 |
|---|---|
| `EXT="JPG"` | 扩展名是 jpg（不区分大小写，不带点） |
| `SIZE<1000000` | 文件小于 1 000 000 字节 |
| `[0]=137 & [1]="P"` | 第 0 个字节是 137，第 1 个字节是字符 P |
| `FIND("<?xml")` | 文件开头的一段内容里包含这个字符串（区分大小写） |
| `FINDI("<html")` | 同上，不区分大小写 |
| `FORCE` | 只在用户手动选择插件时生效 |
| `MULTIMEDIA` | 文件属于 LumaFiles 内置预览支持的媒体类型 |
| `&`  `\|`  `!`  `( )` | 且、或、非、括号。`&` 优先于 `\|` |

例子：

```text
EXT="PSD" | EXT="PSB"
(EXT="XML" | EXT="SVG") & !SIZE>50000000
[0]="P" & [1]="K" & EXT="DOCX"
```

- 空字符串：查看器插件表示对所有文件都尝试；内容插件表示对所有文件提供字段。
- 用户可以在设置页覆盖你的检测字符串。
- 写错语法不会导致插件加载失败，但设置页会标红提示。

---

## 8. 权限

在 `plugin.toml` 的 `[permissions]` 里声明，安装时会征求用户同意。规则沿用 `docs/plugin-permissions.md`，以下几点和插件类型直接相关：

| 权限 | 用途 | 说明 |
|---|---|---|
| （不需要声明） | 读取用户正在查看或处理的文件 | 通过 `blob` 句柄提供；查看器和内容插件一般**什么权限都不需要** |
| `network = ["api.example.com"]` | HTTPS 请求 | 只允许列出的域名 |
| `sockets = ["host:port"]` | 原始 TCP，例如 SFTP、FTP | 本版新增 |
| `edit_fields = true` | 修改文件元数据（内容插件的 `set`） | 宿主只会传入用户选中的那些文件的可写句柄 |
| `read_paths` / `write_paths` | 读写任意本地路径 | 很少需要；需要说明理由，否则商店审核不通过 |

---

## 9. 调试与测试

| 命令 | 作用 |
|---|---|
| `noh plugin test --file <样本>` | 不打开界面，打印内容插件的全部字段值，以及查看器插件 `content()` 的摘要 |
| `noh plugin test --fs --list "\\"` | 在命令行调用文件系统插件列目录 |
| `noh plugin dev` | 开发模式安装到正在运行的 LumaFiles，重新 build 后自动重载；开发模式下的插件在设置页带“开发”标记 |
| `noh plugin probe <dll>` | 检查 TC 原生插件：位数、导出了哪些函数、检测字符串、字段列表（见 §11） |

- 日志：`log()` 的输出写到 `{日志目录}/plugins/<插件id>.log`，设置页的“查看日志”按钮可以直接打开。
- 插件 panic 时，宿主会记录 panic 信息，这次调用按 `other` 错误处理，插件实例会被重建。连续 3 次 panic 后插件会被禁用。
- 建议在 `samples/` 里放真实样本，在 CI 中运行 `noh plugin test`。

---

## 10. 打包与发布

```bash
noh plugin pack        # 生成 linecount-1.0.0.lfp（zip 格式）
```

`.lfp` 包内结构：

```text
plugin.toml
component.wasm
icon.png
i18n/*.toml
README.md
LICENSE
```

- 用户把 `.lfp` 拖进 LumaFiles，或者在设置页点“安装插件包…”即可安装。
- 发布到插件商店的流程见 `docs/plugin-distribution.md`。

---

## 11. 给 Total Commander 插件作者

### 11.1 能直接用吗？
可以。LumaFiles 在独立进程里加载你的 DLL，按 TC 的接口调用（文件系统 2.1 SE、查看器 2.13、内容 2.12）。**只支持 64 位插件**（`.wfx64 / .wlx64 / .wdx64`；PE 头是 x64 的老扩展名 `.wfx / .wlx / .wdx` 也可以）。32 位插件不能加载，设置页会显示原因；请用 64 位工具链重新编译，并提供 W（Unicode）版本的函数。用户可以从 TC 一键导入插件配置，也可以直接拖入带 `pluginst.inf` 的 zip 安装包。

各阶段支持的函数见设计文档 §5.3。没有列出的函数不会被调用。

### 11.2 和在 TC 里运行的区别

| 方面 | 在 LumaFiles 里 |
|---|---|
| 进程 | 你的 DLL 运行在 64 位的 `luma-tcbridge.exe` 里，**不在**文件管理器的主进程里 |
| 线程 | 文件系统和内容插件的调用全部串行；只有你在 `FsGetBackgroundFlags` 里声明支持时，才会在另一个线程上做后台传输。查看器插件的调用都在 UI 线程上 |
| 配置目录 | `*SetDefaultParams` 里的 `DefaultIniName` 指向 `{LumaFiles 配置目录}\plugins\tc\wincmd.ini`。这个文件不一定存在，请只用它所在的目录来放你自己的 ini |
| 环境变量 | `COMMANDER_PATH`、`COMMANDER_INI` 指向上面那个目录（最终以实现为准） |
| 密码 | `CryptProc` 存进 Windows 凭据管理器；LumaFiles 没有 TC 的主密码 |
| 深色模式 | LumaFiles 使用深色主题时，会传入 2.13 的深色模式标志 |
| 查看器窗口 | 父窗口不是 TC 的 Lister。你不处理的按键请按 SDK 约定转发给父窗口，LumaFiles 会处理 `N` / `P` / `Esc` / `F3` / `空格` 等 |
| 主窗口 | 查找 TC 主窗口（`TTOTAL_CMD`）、发送 `cm_*` 命令的功能**不可用** |
| 超时 | 内容插件快速取值超过约 0.5 秒会被当作“延迟”字段；请正确使用 `ft_delayed` |
| 崩溃 | 只影响你自己的进程。10 分钟内崩溃 3 次会被自动禁用，用户可以在设置页看到原因 |

### 11.3 自测
```bash
noh plugin probe D:\plugins\myplugin.wdx64          # 位数、导出函数、检测字符串、字段列表
noh plugin probe D:\plugins\myplugin.wdx64 --file sample.mp4   # 再打印这个文件的每个字段值
```
结果可以和 WDX Test 64（<https://totalcmd.net/plugins/wdxtest64>）在 TC 环境下的输出对照。

### 11.4 建议
- **必须**提供 W（Unicode）版本的函数。LumaFiles 只调用 W 版本，只有 ANSI 函数的插件不能加载。
- 确实很慢的字段，请检查调用标志并返回 `ft_delayed`，不要在快速取值时卡住。
- 发布 zip 时附带 `pluginst.inf`，用户就能拖进来一步安装。

---

## 12. 版本与兼容
- 插件 API 遵循 semver。同一个主版本内（`0.x` 期间是同一个次版本内），宿主保证**只增不减**：新增的接口和字段带默认值，老插件不用重新编译。
- 宿主同时支持当前版本和上一个次版本，例如宿主是 0.3 时，0.2 的插件仍然可以加载。
- 插件的 `api` 比宿主新时拒绝加载，提示用户升级 LumaFiles。
- TC 兼容层的对齐基准只会升级，不会降级。

---

## 13. 常见问题

**能做压缩包插件（类似 WCX）吗？** 已列入路线图 P6，本期不支持。现在可以用文件系统插件把压缩包内容显示成目录来近似实现，但没有“打包”功能。

**能在查看器里嵌入自己的原生窗口吗？** 原生（WASM）插件不能，只能返回文档模型。确实需要原生窗口（比如 WebView2），请写成 TC 查看器插件。

**同一个文件，内置预览和插件谁优先？** 默认**插件优先**，与 TC 一致：只要有查看器插件的检测字符串匹配，就先用插件，图片、PDF、音视频也一样；所有插件都处理不了时才用内置预览。用户可以在设置页改成内置优先。所以请把检测字符串写准确，不要对无法处理的文件返回匹配。

**怎样让用户从 TC 平滑迁移？** 字段名和 TC 版本保持一致；如果同时发布 TC 版和原生版，在两边都使用相同的字段名，用户的列和重命名规则就能继续用。

---

## 参考
- TC 文件系统插件 SDK 2.1 SE：<https://ghisler.github.io/WFX-SDK/contents.htm>
- TC 查看器插件 SDK 2.13：<https://ghisler.github.io/WLX-SDK/contents.htm>
- TC 内容插件 SDK 2.12：<https://ghisler.github.io/WDX-SDK/contents.htm>
- 决策记录：ADR 0010（`docs/adr/0010-tc-plugin-kinds-and-native-bridge.md`）；TC 插件实测结果：`docs/plugin-compat.md`
- LumaFiles 现有插件文档：`docs/plugin-overview.md`、`docs/plugin-permissions.md`、`docs/plugin-distribution.md`、`docs/plugin-templates.md`、ADR 0005（`docs/plugin-api.md` 的 0.1.0 world 已被本文 §3 取代）
