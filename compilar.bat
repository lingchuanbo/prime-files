@echo off
chcp 65001 >nul
setlocal
cd /d "%~dp0"

rem Compila os dois programas que o Prime Files usa:
rem   nohrs.exe               - o explorador
rem   prime-files-indexer.exe - o servico de indexacao (busca instantanea)
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

rem Um Nohrs aberto trava o nohrs.exe do perfil release e a compilacao nao
rem consegue troca-lo. O perfil dist escreve em target\dist, que ninguem usa
rem enquanto o programa roda: so o release precisa fechar. Ele e fechado do
rem jeito normal, que salva as abas (voltam ao abrir); so se nao fechar em
rem alguns segundos e encerrado a forca. find pelo caminho completo: num
rem terminal do Git o nome sozinho e o do Git. `ping` espera um segundo e,
rem ao contrario de `timeout`, funciona com a entrada redirecionada.
if /i "%PROFILE%"=="dist" goto compilar
set "FIND=%SystemRoot%\System32\find.exe"
tasklist /FI "IMAGENAME eq nohrs.exe" 2>nul | "%FIND%" /I "nohrs.exe" >nul
if errorlevel 1 goto compilar
echo Fechando o Nohrs aberto para atualizar...
taskkill /IM nohrs.exe >nul 2>nul
for /L %%I in (1,1,10) do (
  tasklist /FI "IMAGENAME eq nohrs.exe" 2>nul | "%FIND%" /I "nohrs.exe" >nul || goto compilar
  ping -n 2 127.0.0.1 >nul
)
taskkill /F /IM nohrs.exe >nul 2>nul
ping -n 2 127.0.0.1 >nul

:compilar
echo.
echo Compilando o Prime Files e o indexador (perfil %PROFILE%)...
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
rem programa, e o Prime Files le o indice do Everything de la.
if /i "%PROFILE%"=="dist" (
  if exist "target\release\dll" (
    if not exist "%OUT%\dll" mkdir "%OUT%\dll"
    xcopy /e /i /y "target\release\dll" "%OUT%\dll" >nul
  ) else (
    echo AVISO: target\release\dll nao encontrado; o programa nao podera usar o indice do Everything.
  )
)

echo.
echo Programas em %OUT%:
for %%F in ("%OUT%\nohrs.exe" "%OUT%\prime-files-indexer.exe") do (
  if exist "%%~F" (
    echo   %%~nxF  -  %%~zF bytes
  ) else (
    echo   AVISO: %%~nxF nao encontrado.
  )
)
echo.
echo Pronto.
endlocal
