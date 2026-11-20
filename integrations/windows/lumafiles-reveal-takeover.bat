@echo off
chcp 65001 >nul
setlocal

rem 由 Explorer 接收“在文件夹中显示”请求，再由 LumaFiles 监听并接管。
rem 不能阻止发起请求的软件先启动 Explorer。

set "EXE="
if exist "%~dp0LumaFiles.exe" set "EXE=%~dp0LumaFiles.exe"
if not defined EXE if exist "%~dp0..\..\target\release\LumaFiles.exe" set "EXE=%~dp0..\..\target\release\LumaFiles.exe"
if defined EXE for %%I in ("%EXE%") do set "EXE=%%~fI"
set "STATE_DIR=%LOCALAPPDATA%\LumaFiles"
set "STATE_FILE=%STATE_DIR%\reveal-takeover-tool.state"
set "RUN_KEY=HKCU\Software\Microsoft\Windows\CurrentVersion\Run"
set "TOOL_RUN_VALUE=LumaFiles Reveal Takeover"
set "APP_RUN_VALUE=Prime Files"
set "NEW_WINDOW_KEY=HKCU\Software\Classes\CLSID\{52205fd8-5dfb-447d-801a-d0b52f2e83e1}\shell\opennewwindow"

echo.
echo LumaFiles - "在文件夹中显示"接管工具
echo.
echo 1. 启用接管模式
echo 2. 关闭本工具添加的后台启动
echo 3. 退出
choice /c 123 /n /m "请选择 [1-3]: "
if errorlevel 3 exit /b 0
if errorlevel 2 goto disable
goto enable

:enable
if not defined EXE goto no_exe
if exist "%STATE_FILE%" (
  set /p "START_MODE="<"%STATE_FILE%"
  if not defined START_MODE goto state_failed
  goto apply_takeover
)

set "START_MODE="
reg query "%RUN_KEY%" /v "%APP_RUN_VALUE%" 2>nul | findstr /i /c:"LumaFiles.exe" >nul
if not errorlevel 1 set "START_MODE=APP"

if not defined START_MODE (
  reg query "%RUN_KEY%" /v "%TOOL_RUN_VALUE%" >nul 2>nul
  if not errorlevel 1 (
    reg query "%RUN_KEY%" /v "%TOOL_RUN_VALUE%" 2>nul | findstr /i /c:"LumaFiles.exe" >nul
    if errorlevel 1 goto startup_conflict
    set "START_MODE=EXISTING"
  ) else (
    set "START_MODE=CREATED"
    reg add "%RUN_KEY%" /v "%TOOL_RUN_VALUE%" /t REG_SZ /d "\"%EXE%\" --background" /f >nul
    if errorlevel 1 goto registry_failed
  )
)

if not exist "%STATE_DIR%" mkdir "%STATE_DIR%" >nul 2>nul
if not exist "%STATE_DIR%" goto state_failed
>"%STATE_FILE%" echo %START_MODE%
if errorlevel 1 goto state_failed

:apply_takeover
call :remove_default Directory
if errorlevel 1 goto registry_failed
call :remove_default Drive
if errorlevel 1 goto registry_failed
call :remove_new_window
if errorlevel 1 goto registry_failed

start "" "%EXE%" --background
echo.
echo 已启用：Windows Explorer 保持默认，LumaFiles 在后台接管文件位置请求。
echo 请确认 LumaFiles 设置中的“在文件夹中显示”开关已开启。
echo 注意：部分软件仍会短暂启动 Explorer；本工具不能拦截软件直接调用的系统接口。
pause
exit /b 0

:disable
if not exist "%STATE_FILE%" (
  echo 没有找到本工具的启用记录；未修改启动项。
  echo 如 LumaFiles 仍是默认资源管理器，请在 LumaFiles 设置中关闭该选项。
  pause
  exit /b 0
)

set "START_MODE="
set /p "START_MODE="<"%STATE_FILE%"
if /i "%START_MODE%"=="CREATED" (
  reg query "%RUN_KEY%" /v "%TOOL_RUN_VALUE%" 2>nul | findstr /i /c:"LumaFiles.exe" >nul
  if not errorlevel 1 reg delete "%RUN_KEY%" /v "%TOOL_RUN_VALUE%" /f >nul
)
call :remove_default Directory
call :remove_default Drive
call :remove_new_window
del "%STATE_FILE%" >nul 2>nul
echo 已关闭本工具添加的后台启动记录。
echo Windows 文件夹关联保持为系统默认；不会恢复 LumaFiles 默认资源管理器设置。
pause
exit /b 0

:remove_default
set "SHELL_KEY=HKCU\Software\Classes\%~1\shell"
set "COMMAND="
for /f "tokens=2,*" %%A in ('reg query "%SHELL_KEY%\nohrs\command" /ve 2^>nul ^| findstr /i "REG_SZ"') do set "COMMAND=%%B"
if not defined COMMAND exit /b 0
echo(%COMMAND%|findstr /i /c:"LumaFiles.exe" >nul
if errorlevel 1 exit /b 0
reg delete "%SHELL_KEY%\nohrs" /f >nul 2>nul
if errorlevel 1 exit /b 1
for /f "tokens=2,*" %%A in ('reg query "%SHELL_KEY%" /ve 2^>nul ^| findstr /i "REG_SZ"') do (
  if /i "%%B"=="nohrs" reg delete "%SHELL_KEY%" /ve /f >nul 2>nul
)
exit /b 0

:remove_new_window
set "COMMAND="
for /f "tokens=2,*" %%A in ('reg query "%NEW_WINDOW_KEY%\command" /ve 2^>nul ^| findstr /i "REG_SZ"') do set "COMMAND=%%B"
if not defined COMMAND exit /b 0
echo(%COMMAND%|findstr /i /c:"LumaFiles.exe" >nul
if errorlevel 1 exit /b 0
reg delete "%NEW_WINDOW_KEY%" /f >nul 2>nul
exit /b %errorlevel%

:startup_conflict
echo 启动项 "%TOOL_RUN_VALUE%" 已被其他程序占用；未覆盖它。
pause
exit /b 1

:registry_failed
echo 修改注册表失败。请检查当前用户权限及注册表状态。
echo 若后台启动项已添加，可再次运行本工具选择关闭。
pause
exit /b 1

:state_failed
if /i "%START_MODE%"=="CREATED" reg delete "%RUN_KEY%" /v "%TOOL_RUN_VALUE%" /f >nul 2>nul
echo 无法写入状态文件："%STATE_FILE%"。
echo 未能安全记录恢复状态；请检查用户配置目录权限。
pause
exit /b 1

:no_exe
echo 未找到 LumaFiles.exe。
echo 请将本工具放在 LumaFiles.exe 旁边，或在仓库中确保 target\release\LumaFiles.exe 存在。
pause
exit /b 1
