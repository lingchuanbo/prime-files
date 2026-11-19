@echo off
chcp 65001 >nul
setlocal
rem Faz pastas e drives abrirem no Nohrs: duplo clique na Area de Trabalho,
rem "Abrir pasta" de outros programas, atalhos para pastas. Vale so para o seu
rem usuario (HKCU), nao pede administrador, e desfaz-padrao.bat volta tudo.
rem
rem O Windows chama "LumaFiles.exe <pasta>"; se o LumaFiles ja estiver aberto, a pasta
rem abre numa aba nova da mesma janela.
rem
rem Nao cobre: Win+E e o botao do Explorador na barra de tarefas, que abrem o
rem explorer.exe direto, e pastas especiais (Este Computador, Lixeira).

for %%I in ("%~dp0..\..\target\release\LumaFiles.exe") do set "EXE=%%~fI"
if not exist "%EXE%" (
  echo Nao achei %EXE%.
  echo Rode iniciar-nohrs.bat uma vez para compilar.
  pause
  exit /b 1
)

call :registrar Directory || goto falhou
call :registrar Drive || goto falhou

echo Pronto: pastas e drives abrem no Nohrs.
echo Para voltar ao Explorador: desfaz-padrao.bat
pause
exit /b 0

:registrar
set "CHAVE=HKCU\Software\Classes\%~1\shell"
reg add "%CHAVE%\nohrs" /ve /d "Abrir no Nohrs" /f >nul || exit /b 1
reg add "%CHAVE%\nohrs" /v Icon /d "\"%EXE%\"" /f >nul || exit /b 1
reg add "%CHAVE%\nohrs\command" /ve /d "\"%EXE%\" \"%%1\"" /f >nul || exit /b 1
rem O valor padrao de "shell" escolhe o verbo do duplo clique.
reg add "%CHAVE%" /ve /d "nohrs" /f >nul || exit /b 1
exit /b 0

:falhou
echo Nao consegui gravar no registro.
echo Rode desfaz-padrao.bat para limpar o que tiver ficado.
pause
exit /b 1
