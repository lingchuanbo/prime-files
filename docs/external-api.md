# 轻寻文件外部调用

## 启动参数

发布版程序名为 `LumaFiles.exe`，请将示例路径替换为实际安装路径。

```powershell
& 'C:\LumaFiles\LumaFiles.exe' open 'D:\工作资料'
& 'C:\LumaFiles\LumaFiles.exe' open 'D:\工作资料\说明.txt'
& 'C:\LumaFiles\LumaFiles.exe' current-path
```

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

响应的 `ok` 为 `false` 表示失败，部分失败响应含 `error`。标签页 ID 仅在当前进程内有效；每次调用应重新读取端点，设置连接和读取超时，并处理窗口关闭或启动中的情况。IPC 路径由运行实例的配置模式决定，而不是外部调用者的当前工作目录。
