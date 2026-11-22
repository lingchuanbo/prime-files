# LumaFiles 快速启动面板（Quick Dock）方案

> 目标：在 LumaFiles（prime-files）里加一个“分组 + 图标格子”的软件快速启动面板，功能对标参考截图。配置跟 LumaFiles 走：同一套配置目录（含便携模式 `userdata/`）、同一套主题和 i18n，在同一个设置页里编辑。
> 依据：`docs/config.md`（`config.toml` 加拆分文件、便携模式）、`docs/launcher.md`（现有 Raycast 风搜索启动器）、`crates/nohrs-launcher`、`crates/nohrs-core/src/config`。

---

## 1. 定位：和现有 Launcher 的关系

| | 现有 Launcher（`nohrs-launcher`） | 新增 Quick Dock |
|---|---|---|
| 交互 | 敲键盘搜索 | 鼠标或快捷键点图标 |
| 内容 | 文件、命令、搜索结果 | 用户自己整理的程序、文件夹、网址、脚本 |
| 关系 | Dock 里的条目注册成 Launcher 的数据源，在 Launcher 里输入 `@ps` 也能启动 Photoshop | 共用一套“启动动作”（Action） |

放在 `nohrs-launcher` 里新增 `dock/` 模块，不另开 crate。启动逻辑放进 `nohrs-services/src/launch/`，GUI、CLI 和插件都能调用。

---

## 2. 功能清单

### 2.1 界面（对应截图）
- **左侧分组栏**：工作、通用、工具、系统、TOTALCMD、游戏、项目……可以增删、改名、拖动排序，可以给分组设图标，选中项有下划线高亮。
- **两种视图**，标题栏按钮切换：
  - **网格**：大图标加名称，列数和格子大小可调，长名字截断后悬停显示全名。
  - **列表**：小图标加全名，单列或双列。
- **空位可保留**：截图里有空格子用来分隔。按“槽位”存储，拖到哪里就在哪里。
- **窗口**：最小化、**置顶（图钉）**、关闭；可以设置失去焦点自动隐藏；记住每个显示器上的位置和尺寸（存在 `state.redb`）。
- **呼出方式**：全局热键（默认 `Ctrl+Alt+Space`，可改）、托盘菜单、LumaFiles 工具栏按钮、鼠标贴屏幕边缘或中键，也可以停靠在桌面边缘做成抽屉。

### 2.2 条目操作
- 从 LumaFiles 文件列表、桌面或开始菜单**直接拖进来**，自动识别 `.exe`、`.lnk`、`.url`、文件夹和 UWP 应用。
- 右键菜单：以管理员运行、打开所在位置（在 LumaFiles 里打开）、编辑、复制为新条目、移到其他分组、删除。
- `Ctrl+点击`以管理员运行，`Shift+点击`打开所在位置，中键用自定义的第二动作。
- **键盘**：方向键移动，`Enter` 启动，输入字母就地过滤，`1`～`9` 或 `Alt+数字` 切换分组。
- 条目找不到目标时显示灰色角标，可一键重新定位。
- 多选后批量移动或删除。

### 2.3 条目类型（可扩展）

| 类型 | 说明 |
|---|---|
| `app` | exe 或 lnk，可设参数、工作目录、以管理员运行、窗口状态、环境变量 |
| `folder` | 在 LumaFiles 新标签中打开（不交给资源管理器） |
| `file` | 用默认程序或指定程序打开 |
| `url` | 浏览器打开，可指定浏览器 |
| `uwp` | 通过 AppUserModelID 启动 |
| `command` | 运行命令或脚本（cmd、pwsh、python），可选隐藏窗口 |
| `group` | 一次启动多个条目（比如“打开 Houdini + PureRef + 笔记”） |
| `plugin:<id>` | 插件提供的自定义动作（见第 5 节） |

**已在运行时**：可以选择激活已有窗口、启动新实例或询问，默认激活已有窗口。

---

## 3. 配置：跟随 LumaFiles

### 3.1 位置
沿用 `docs/config.md` 的拆分文件机制，新增 **`dock.toml`**，和 `appearance.toml`、`search.toml` 放在同一目录：
- 普通安装：LumaFiles 的配置目录。
- 便携模式：`userdata/dock.toml`，图标缓存放在 `userdata/cache/dock-icons/`。
- 切换配置档时，按现有规则复制缺失的文件，不覆盖已有文件。
- 窗口位置、使用次数这类高频数据写进 `state.redb`，不放在 toml 里（同 launcher 的做法）。

### 3.2 示例 `dock.toml`

```toml
schema_version = 1

[dock]
hotkey        = "Ctrl+Alt+Space"
view          = "grid"          # grid | list
columns       = 8
cell_size     = 96
icon_size     = 48
auto_hide     = true            # 失焦隐藏
topmost       = false
on_running    = "activate"      # activate | new | ask
theme         = "inherit"       # 跟随 LumaFiles 的 appearance.toml

[[group]]
id    = "work"
name  = "工作"                   # 也可用 "i18n:dock.work"
icon  = "fluent:briefcase"

  [[group.item]]
  slot  = 0
  type  = "app"
  name  = "After Effects 2024"
  path  = 'C:\Program Files\Adobe\Adobe After Effects 2024\Support Files\AfterFX.exe'
  admin = false
  hotkey = "Ctrl+Alt+A"         # 可选，条目级全局热键
  tags  = ["ae", "adobe"]       # 供 Launcher 搜索

  [[group.item]]
  slot  = 2                     # slot 1 留空 = 空格子
  type  = "group"
  name  = "建模套件"
  items = ["houdini", "pureref", "notes"]

  [[group.item]]
  slot = 3
  type = "folder"
  name = "项目"
  path = '%USERPROFILE%\Projects'  # 支持环境变量和 {portable} 等变量
```

- 路径支持 `%ENV%`、`{portable}`（便携根目录）和 `{luma}`（程序目录），方便 U 盘或换机后直接用。
- 用 `serde` 加上现有 `config/watcher.rs`，改文件后**热重载**；解析失败时保留旧配置，并在界面上提示出错的行号。
- 补进 `docs/config.schema.json`，在编辑器里写 `dock.toml` 有补全和校验。
- 支持导入和导出：导入 PowerToys Run、Listary、RunAny、Rolan 的配置，导出单个 `dock.toml`。

### 3.3 设置页
在现有 `nohrs-pages/src/settings.rs` 里加一个 **“快速启动”** 页：分组和条目的可视化编辑、热键录制、视图参数，以及“打开 dock.toml”按钮。界面上的改动直接写回 toml。

---

## 4. 实现结构

```
crates/
├── nohrs-core/src/config/dock.rs        # DockConfig 模型、加载、校验、迁移 (schema_version)
├── nohrs-services/src/launch/
│   ├── mod.rs                           # Launcher trait + 注册表
│   ├── win32.rs                         # ShellExecuteExW / CreateProcessW / runas / UWP
│   ├── running.rs                       # 查找已运行进程并激活窗口
│   └── icon.rs                          # 图标提取 (SHGetImageList / IShellItemImageFactory) + 磁盘缓存
├── nohrs-launcher/src/dock/
│   ├── window.rs                        # 窗口、置顶、自动隐藏、位置记忆
│   ├── view_grid.rs / view_list.rs      # 两种视图 (gpui uniform_list 虚拟化)
│   ├── dnd.rs                           # 拖入/重排 (复用 third_party/gpui 修过的 drop target)
│   └── source.rs                        # 注册为 Launcher 数据源
└── nohrs-cli                            # `noh dock add|list|run <name>` 方便脚本化
```

**核心 trait（扩展点）：**

```rust
pub trait LaunchAction: Send + Sync {
    fn kind(&self) -> &'static str;                     // "app" | "url" | ...
    fn icon(&self, item: &DockItem) -> IconSource;
    fn launch(&self, item: &DockItem, ctx: &LaunchCtx) -> Result<LaunchOutcome>;
    fn context_menu(&self, item: &DockItem) -> Vec<MenuEntry> { vec![] }
    fn validate(&self, item: &DockItem) -> Health { Health::Ok } // 失效检测
}
```

内置类型和插件类型都走同一个注册表，新增类型时不用改界面代码。

### 性能与稳定性要点（承接上一份优化方案）
- 启动动作和图标提取都**不在 UI 线程**上运行。图标先显示占位，异步加载后替换；缓存键为路径、修改时间和尺寸。
- 冷启动时不读任何 exe，只读图标缓存，目标是热键按下后 **50 ms 内**出现窗口；面板常驻，隐藏时不销毁。
- COM 和 Shell 调用放在 `catch_unwind` 里，出错只影响单个条目。
- 失效检测放到后台空闲时批量执行，不阻塞显示。
- 配置文件原子写入（先写临时文件再 rename），每次保存前备份到 `dock.toml.bak`。

---

## 5. 扩展性
1. **插件**：按 `docs/plugin-api.md` 的 WIT 组件模型，插件可以注册新的条目类型（比如“SSH 主机”“Steam 游戏”“Docker 容器”）、动态分组（比如自动扫描 Steam 库、最近项目）和右键菜单项。
2. **动态分组**：分组可以设 `source = "folder:D:\\Tools"`（自动列出文件夹里的程序），或者 `source = "recent"`、`source = "most_used"`。
3. **规则**：按当前前台程序或时间自动切换默认分组，比如打开 Houdini 时默认显示“项目”。
4. **同步**：`dock.toml` 是纯文本，可以放进网盘或 Git，便携模式下直接随程序走。

---

## 6. 分阶段交付

| 阶段 | 内容 | 预估 |
|---|---|---|
| M1 | `dock.toml` 模型和热重载；窗口、分组栏、网格视图；app、folder、url 三种类型；全局热键；图标缓存 | 1～1.5 周 |
| M2 | 拖入和重排、空槽、列表视图、右键菜单、管理员运行、激活已运行程序、键盘操作 | 1 周 |
| M3 | 设置页可视化编辑、导入导出、失效检测、`noh dock` CLI、Launcher 数据源 | 1 周 |
| M4 | `group`、`command`、`uwp` 类型，动态分组，插件扩展点，边缘停靠 | 1～2 周 |

**验收标准**：热键到窗口出现 ≤ 50 ms；200 个条目的分组滚动不掉帧；条目路径失效、图标损坏或配置写坏时都不崩溃，并给出提示；便携模式换机后无需修改即可使用。
