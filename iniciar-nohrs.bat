@echo off
chcp 65001 >nul
setlocal
cd /d "%~dp0"

rem O tema vem do config (escuro por padrao) e troca pelo menu do botao
rem direito, em Exibir > Tema. Um --theme aqui travaria essa escolha.

where cargo >nul 2>nul
if errorlevel 1 (
  echo Rust nao encontrado. Instale em https://rustup.rs e abra este arquivo de novo.
  pause
  exit /b 1
)

rem Um LumaFiles aberto trava o LumaFiles.exe e a compilacao nao consegue troca-lo.
rem Ele e fechado do jeito normal, que salva as abas (voltam ao abrir); so se
rem nao fechar em alguns segundos e encerrado a forca. find e timeout pelo
rem caminho completo: num terminal do Git os nomes sozinhos sao os do Git.
set "FIND=%SystemRoot%\System32\find.exe"
set "WAIT=%SystemRoot%\System32\timeout.exe"
tasklist /FI "IMAGENAME eq LumaFiles.exe" 2>nul | "%FIND%" /I "LumaFiles.exe" >nul
if errorlevel 1 goto compilar
echo Fechando o LumaFiles aberto para atualizar...
taskkill /IM LumaFiles.exe >nul 2>nul
for /L %%I in (1,1,10) do (
  tasklist /FI "IMAGENAME eq LumaFiles.exe" 2>nul | "%FIND%" /I "LumaFiles.exe" >nul || goto compilar
  "%WAIT%" /t 1 /nobreak >nul
)
taskkill /F /IM LumaFiles.exe >nul 2>nul
"%WAIT%" /t 1 /nobreak >nul

:compilar
rem Release porque a build de debug fica visivelmente lenta para rolar pastas
rem grandes. A primeira vez leva alguns minutos; depois so recompila o que mudou.
echo Compilando o LumaFiles (release)...
cargo build --release -p nohrs
if errorlevel 1 (
  echo.
  echo A compilacao falhou. A mensagem de erro esta acima.
  pause
  exit /b 1
)

echo Abrindo o LumaFiles...
start "" "%~dp0target\release\LumaFiles.exe"
endlocal
