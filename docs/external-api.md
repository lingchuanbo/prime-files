# 轻寻文件外部调用

## 启动参数

发布版程序名为 `LumaFiles.exe`，请将示例路径替换为实际安装路径。

```powershell
& 'C:\LumaFiles\LumaFiles.exe' open 'D:\工作资料'
& 'C:\LumaFiles\LumaFiles.exe' open 'D:\工作资料\说明.txt'
& 'C:\LumaFiles\LumaFiles.exe' current-path
& 'C:\LumaFiles\LumaFiles.exe' -quick_launcher   # 打开或关闭快速启动面板
& 'C:\LumaFiles\LumaFiles.exe' -search           # 打开或关闭独立搜索窗口
```

`-quick_launcher` 和 `-search`（也接受 `--quick_launcher`、`--search`，以及不带前面的 `quick_launcher`、`search`）每次执行都切换对应窗口：程序已经在运行时由运行中的实例打开或关闭，本次调用随即退出，**不会多开进程**；程序没有运行时则先启动程序，再在主窗口之后打开该窗口。所以脚本里重复调用同一条命令即可，例如 AutoHotkey 的 `Run "C:\LumaFiles\LumaFiles.exe -quick_launcher"`。

`open` 接受文件夹或文件：文件会打开所在文件夹并选中该文件。已有窗口时在新标签页打开；否则启动程序。路径不存在时返回非零退出码。原有的直接传入路径仍可使用。

`current-path` 查询运行中窗口的活动资源管理器标签页，将路径输出到标准输出，不启动新窗口；窗口未运行或无法查询时返回非零退出码并输出错误。Windows 发布版是 GUI 程序，终端可能不会等待其结束；脚本建议使用带 stdout/stderr 重定向并等待进程退出的进程 API，或直接使用下述 IPC。虚拟位置可能不是文件系统路径。

## 本地 IPC

程序运行时，端点文件通常位于 `%LOCALAPPDATA%\nohrs\ipc.json`；绿色模式位于程序同级 `userdata\runtime\ipc.json`。其中 `port` 是本机 TCP 端口，`token` 是每次启动生成的认证令牌。文件可能在退出后遗留，应以连接及响应结果判断程序是否运行。不要向其他用户或远程服务泄露令牌。

连接 `127.0.0.1:port`，使用 UTF-8 发送一行 JSON（以换行结束），读取一行 JSON 响应，然后关闭连接。每次请求均须携带 `token`。例如（路径在 JSON 中应转义）：

```json
{"token":"端点文件中的令牌","cmd":"current"}
{"ok":true,"tab":123,"folder":"D:\\工作资料"}
```

支持的请求：

- `ping`：检查窗口是否响应。
- `show`：激活窗口。
- `open`：传入绝对路径 `path`，打开文件夹或选中文件，并激活窗口。
- `current`：返回活动标签页的 `tab` 和 `folder`。
- `tabs`：返回 `tabs` 数组，每项包含 `tab`、`folder` 和 `active`。
- `navigate`：传入 `tab` 和绝对路径 `path`，在指定标签页导航。
- `toggle_quick_launch`：打开或关闭快速启动面板（与 `-quick_launcher` 相同）。
- `toggle_search`：打开或关闭独立搜索窗口（与 `-search` 相同）。

响应的 `ok` 为 `false` 表示失败，部分失败响应含 `error`。标签页 ID 仅在当前进程内有效；每次调用应重新读取端点，设置连接和读取超时，并处理窗口关闭或启动中的情况。IPC 路径由运行实例的配置模式决定，而不是外部调用者的当前工作目录。

## 窗口类名

AutoHotkey 等外部脚本可以用窗口类名（`ahk_class`）定位本程序的窗口，不必再靠标题或进程号区分。程序里的 GPUI 窗口各用各的类名：

| 窗口 | 窗口类名 |
|---|---|
| 主窗口（资源管理器） | `LumaFiles_WinClass` |
| 快速启动面板 | `LumaFiles_QuickLauncher_WinClass` |
| 独立搜索窗口（`Ctrl+Shift+F`） | `LumaFiles_Search_WinClass` |
| 启动器面板（`Ctrl+Shift+Space`，程序内 `Ctrl+K`） | `LumaFiles_Launcher_WinClass` |
| 批量重命名窗口 | `LumaFiles_BatchRename_WinClass` |
| 常驻模式的隐藏窗口 | `LumaFiles_KeepAlive_WinClass` |

```ahk
; AutoHotkey v2
if WinExist("ahk_class LumaFiles_WinClass")
    WinActivate
WinActivate "ahk_class LumaFiles_Search_WinClass"
```

托盘、跳转栏和 gpui 的隐藏辅助窗口不在上表中，它们的类名分别是 `LumaFiles.Tray`、`LumaFiles.DialogJump.Bar`、`LumaFiles_PlatformWindow`。`ahk_exe LumaFiles.exe` 一直可用；从旧版本升级的脚本需要把 `ahk_class Zed::Window` 改成上表中的名字。类名在 [nohrs-core 的 `window_class`](../crates/nohrs-core/src/window_class.rs) 里集中定义，改名等于改接口，脚本也会跟着失效。
