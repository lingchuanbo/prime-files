# 0010 — 插件按 Total Commander 三类划分，在 WASM 原生路线之外加一条进程外 TC 兼容路线

> Status: Accepted（2026-11-22，BoBO 确认 Q2、Q3、Q5；Q1、Q6、Q7 按默认做法执行）
> Date: 2026-11-22
> Supersedes: 无。**扩展** ADR 0005，不推翻它
> Related: [`docs/LumaPlugin-设计与边界约定.md`](../LumaPlugin-设计与边界约定.md)、[`docs/LumaPlugin-SDK开发指南.md`](../LumaPlugin-SDK开发指南.md)、[ADR 0004](./0004-remove-tokio.md)、[ADR 0005](./0005-wit-bindgen-component-model.md)

## Context

- 这个 fork（LumaFiles）的用户大多从 Total Commander 迁移过来，手里有现成的 TC 插件：文件系统插件（WFX）、查看器插件（WLX）、内容插件（WDX）。TC 生态里这三类插件数量很多，自己重写不现实。
- ADR 0005 定下的原生插件路线（wasmtime + WASI Preview 2 + Component Model）解决了沙箱和跨语言的问题，但：
  - `docs/plugin-api.md`（`nohrs:plugin@0.1.0`）按 Raycast 风格划分为 commands / decorations / previews / events，和 TC 的三类不对应，资源管理器里的“列”“虚拟文件系统”没有对应的扩展点；
  - WASM 加载不了 TC 的原生 DLL。
- TC 插件是 Win32 DLL，导出 C 函数，需要真实的 HWND（查看器）和串行调用（文件系统、内容），而且大量插件不是线程安全的。
- 主程序的工作区设置了 `unsafe_code = deny`。第三方 DLL 一旦崩溃、卡死或泄漏内存，就会拖垮整个文件管理器。
- 截至 2026-11 的 TC 官方 SDK 版本：文件系统插件 2.1 SE、查看器插件 2.13、内容插件 2.12。

## Decision

1. **插件类型按 TC 划分为三类**：文件系统 `FsProvider`、查看器 `Viewer`、内容 `ContentProvider`。界面层（资源管理器、预览、批量重命名、搜索、设置页）只依赖这三个 trait，放在新 crate `nohrs-plugins` 里。
2. **两个后端**：
   - `TcBackend`：加载 TC 原生插件，**只支持 64 位**（`.wfx64 / .wlx64 / .wdx64`，以及 PE 头为 x64 的旧扩展名），只调用 W（Unicode）函数；
   - `WasmBackend`：原生插件，沿用 ADR 0005 的运行时，WIT 改为 `lumafiles:plugin@0.2.0`，包含 `fs-plugin`、`viewer-plugin`、`content-plugin` 三个 world。`nohrs:plugin@0.1.0` 还没有实现，也没有用户，直接作废。
3. **TC 插件在进程外运行**：每个 DLL 一个 `luma-tcbridge.exe` 进程，通过命名管道和主进程通信，用 Job Object 保证随主进程退出；崩溃后自动重启，10 分钟内崩溃 3 次就自动禁用。`unsafe` 只出现在桥接 crate 里，主程序保持 `deny`。
4. **检测字符串采用 TC 语法**（`EXT=` / `SIZE` / `[n]` / `FIND` / `FORCE` / `MULTIMEDIA`），由宿主统一解析，两条路线共用。
5. **查看器默认插件优先**（Q2）：只要有插件匹配就先用插件，所有插件都处理不了时才用内置预览；预览面板最多等待 300 ms，超时先显示内置预览。
6. **配置跟着 LumaFiles 走**：新增 `plugins.toml` 和 `columns.toml`，放在当前配置目录（便携模式下在 `userdata/`）。TC 插件的 ini 隔离在 `{config}/plugins/tc/`，绝不读写用户真实的 TC 配置；插件凭据存进 Windows 凭据管理器。
7. **阶段**：P0 技术验证 → P1 内容插件（包含列集）→ P2 查看器插件 → P3 文件系统插件 → P4 WASM 原生插件 → P5 生态 → P6 压缩包插件（WCX，Q5）。

## Consequences

### Positive

- 用户现有的 64 位 TC 插件不用修改就能使用，可以从 `wincmd.ini` 一键导入。
- 第三方原生代码的崩溃和卡死被限制在桥接进程里，主程序的稳定性不受影响。
- 资源管理器、批量重命名、搜索只需要对接一套接口，两条路线新增的能力自动可用。
- 插件作者的心智模型和 TC 一致（三类插件、检测字符串、字段名），迁移成本低。

### Negative

- 多了一个可执行文件（`luma-tcbridge.exe`）和一套管道协议要维护、发布、签名。
- 每次调用都多一次 IPC。内容插件取值的额外开销预算为 0.5 ms，必须靠批量请求和缓存控制住。
- 查看器嵌进预览面板需要跨进程的父子窗口，两边会互相牵连；必须先做技术验证（设计文档 §9.2 S1），验证不通过就退回到“静态缩略图 + F3 独立窗口”。
- 32 位插件不支持（Q3），一部分老插件用不了。
- TC 兼容层只能在 Windows 上使用；其他平台只有 WASM 原生插件。

## Alternatives Considered

| 方案 | 不采用的原因 |
|---|---|
| 在主进程里直接 `LoadLibrary` TC 插件 | 一个插件崩溃就拖垮整个程序；违反 `unsafe_code = deny`；不同插件的线程模型无法隔离 |
| 所有 TC 插件共用一个桥接进程 | 一个插件卡死会影响其他所有插件。保留为可选配置（`per_plugin_process = false`），默认不用 |
| 只做 WASM 原生插件，不兼容 TC | 失去现成的插件生态，用户迁移成本高 |
| 只做 TC 兼容，不做 WASM | 没有沙箱，只能在 Windows 上用，插件作者只能写 C/C++/Delphi |
| 同时支持 32 位插件（额外构建 x86 桥接） | Q3 决定只支持 64 位：少一个构建目标，也少一套 ANSI 编码兼容 |
| 保留 `nohrs:plugin@0.1.0` 的 commands/decorations/previews 划分 | 和 TC 的三类不对应，没有“列”和“虚拟文件系统”的扩展点 |
