@echo off
chcp 65001 >nul
setlocal
cd /d "%~dp0"

rem Compila os dois programas que o LumaFiles usa:
rem   LumaFiles.exe           - o explorador
rem   luma-files-indexer.exe - o servico de indexacao (busca instantanea)
rem
rem   compilar.bat        -> perfil release, o do dia a dia (recompila rapido)
rem   compilar.bat dist   -> perfil dist, o que o instalador empacota
rem
rem O instalador (installer\prime-files.iss) espera os dois em target\dist.

set "PROFILE=release"
set "OUT=target\release"
if /i "%~1"=="dist" (
  set "PROFILE=dist"
  set "OUT=target\dist"
)

where cargo >nul 2>nul
if errorlevel 1 (
  echo Rust nao encontrado. Instale em https://rustup.rs e abra este arquivo de novo.
  pause
  exit /b 1
)

rem Um LumaFiles aberto trava o LumaFiles.exe do perfil release e a compilacao nao
rem consegue troca-lo. O perfil dist escreve em target\dist, que ninguem usa
rem enquanto o programa roda: so o release precisa fechar. Ele e fechado do
rem jeito normal, que salva as abas (voltam ao abrir); so se nao fechar em
rem alguns segundos e encerrado a forca. find pelo caminho completo: num
rem terminal do Git o nome sozinho e o do Git. `ping` espera um segundo e,
rem ao contrario de `timeout`, funciona com a entrada redirecionada.
if /i "%PROFILE%"=="dist" goto compilar
set "FIND=%SystemRoot%\System32\find.exe"
tasklist /FI "IMAGENAME eq LumaFiles.exe" 2>nul | "%FIND%" /I "LumaFiles.exe" >nul
if errorlevel 1 goto compilar
echo Fechando o LumaFiles aberto para atualizar...
taskkill /IM LumaFiles.exe >nul 2>nul
for /L %%I in (1,1,10) do (
  tasklist /FI "IMAGENAME eq LumaFiles.exe" 2>nul | "%FIND%" /I "LumaFiles.exe" >nul || goto compilar
  ping -n 2 127.0.0.1 >nul
)
taskkill /F /IM LumaFiles.exe >nul 2>nul
ping -n 2 127.0.0.1 >nul

:compilar
echo.
echo Compilando o LumaFiles e o indexador (perfil %PROFILE%)...
echo.
if /i "%PROFILE%"=="dist" (
  cargo build --profile dist -p nohrs -p nohrs-indexer
) else (
  cargo build --release -p nohrs -p nohrs-indexer
)
if errorlevel 1 (
  echo.
  echo A compilacao falhou. A mensagem de erro esta acima.
  pause
  exit /b 1
)

rem O SDK do Everything (as dlls que o usuario coloca em target\release\dll)
rem vai junto no perfil dist: o instalador embala a pasta dll ao lado do
rem programa, e o LumaFiles le o indice do Everything de la.
if /i "%PROFILE%"=="dist" (
  if exist "target\release\dll" (
    if not exist "%OUT%\dll" mkdir "%OUT%\dll"
    xcopy /e /i /y "target\release\dll" "%OUT%\dll" >nul
  ) else (
    echo AVISO: target\release\dll nao encontrado; o programa nao podera usar o indice do Everything.
  )
  rem HatchZip (MIT/Apache-2.0, https://github.com/xevrion/hatchzip): o extrator
  rem que acompanha o programa. O instalador embala a pasta tools ao lado do exe.
  if exist "target\release\tools" (
    if not exist "%OUT%\tools" mkdir "%OUT%\tools"
    xcopy /e /i /y "target\release\tools" "%OUT%\tools" >nul
  ) else (
    echo AVISO: target\release\tools nao encontrado; o HatchZip nao sera empacotado.
  )
)

echo.
echo Programas em %OUT%:
for %%F in ("%OUT%\LumaFiles.exe" "%OUT%\luma-files-indexer.exe") do (
  if exist "%%~F" (
    echo   %%~nxF  -  %%~zF bytes
  ) else (
    echo   AVISO: %%~nxF nao encontrado.
  )
)
echo.
echo Pronto.
endlocal
