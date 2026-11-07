@echo off
REM ===========================================================================
REM  Agent2API server launcher (Windows)
REM
REM  Double-click it, or run it from a console:
REM    start-plugin-server.bat
REM    start-plugin-server.bat -DryRun
REM    start-plugin-server.bat -MigrateData
REM    start-plugin-server.bat -DataHome "E:\agent2api-data"
REM    start-plugin-server.bat --help
REM
REM  This is a thin wrapper. Port, data directory, panel account and every
REM  environment variable live in start-plugin-server.ps1; this batch only
REM  resolves the arguments, checks the binary is there, and hands off. Change
REM  behaviour there, not here.
REM
REM  Arguments are forwarded verbatim to the PowerShell script, so anything the
REM  script accepts works here too. --help is the one exception: it is answered
REM  locally without starting PowerShell.
REM
REM  Before it launches, the script refuses to continue when:
REM    - start-plugin-server.ps1 is missing
REM    - target\release\agent2api-server.exe is missing
REM      (run build-server.bat first)
REM  The server itself reports a bind error if the configured port is occupied.
REM
REM  Encoding contract: ASCII text, CRLF line endings, nothing else.
REM  cmd.exe mis-parses UTF-8 or GBK bytes in a batch file, and it also
REM  mis-parses LF-only line endings. Do not put non-ASCII characters here and
REM  do not let an editor save it as LF. All Chinese text comes from the .ps1,
REM  which carries its own BOM.
REM ===========================================================================
setlocal EnableExtensions EnableDelayedExpansion

cd /d "%~dp0"

REM Capture the script name before any shift. shift moves %0 along with the
REM arguments, so a later %~nx0 would name whatever argument landed in slot 0.
set "ME=%~nx0"

set "PS1=%~dp0start-plugin-server.ps1"
set "EXE=%~dp0target\release\agent2api-server.exe"

REM A double-clicked batch runs in a console that closes as soon as the script
REM ends, which is exactly when you most want to read the failure. The invoking
REM command line contains this script's own name in that case, so pause at the
REM end to keep it visible. Launched from an existing console the name is not
REM there and nothing pauses.
REM
REM Pure-batch substring test on purpose: shelling out to find.exe would be
REM resolved against the caller's PATH, where a POSIX find earlier in PATH
REM takes /i as a path and fails.
set "DO_PAUSE=0"
set "_CMD=!cmdcmdline!"
if not "!_CMD:%ME%=!"=="!_CMD!" set "DO_PAUSE=1"
REM Automation escape hatch, e.g. CI or a wrapper script.
if defined AGENT2API_NO_PAUSE set "DO_PAUSE=0"

set "ARGS="

REM Arguments are forwarded through positional slots A1..A9, not by appending to
REM one ARGS string. Building a string and expanding it later loses the quotes:
REM a path with spaces is split into separate arguments by the time PowerShell
REM sees it. Slots survive because each one is quoted at use time.
set "N=0"

:parse
if "%~1"=="" goto launch
REM Whitespace-only arguments are ignored, not forwarded. Explorer launches a
REM double-clicked .bat as: cmd /c ""C:\path\start-plugin-server.bat" " -- note
REM the trailing quoted space. cmd hands that to the script as a real first
REM argument whose value is a single space. Forwarding it would make PowerShell
REM fail with "a positional parameter cannot be found".
set "_A=%~1"
if "!_A: =!"=="" (
    shift
    goto parse
)
if /i "%~1"=="--help" goto usage
if /i "%~1"=="-h"     goto usage
if /i "%~1"=="/?"     goto usage
set /a N+=1
if !N! GTR 9 (
    echo [ERROR] Too many arguments, 9 is the limit for this wrapper.
    set "RC=2"
    goto done
)
set "A!N!=%~1"
shift
goto parse

:usage
echo Usage: %ME% [options passed to start-plugin-server.ps1]
echo.
echo   (no option)                  start the gateway (address comes from the .ps1)
echo   -DryRun                      print the resolved config, start nothing
echo   -MigrateData                 copy legacy data into the new data dir
echo   -DataHome ^<path^>             override the data directory
echo   --help                       show this text
echo.
echo Port, data directory and panel account live in start-plugin-server.ps1.
set "RC=0"
goto done

:launch
if not exist "%PS1%" (
    echo [ERROR] Missing launcher: %PS1%
    set "RC=1"
    goto done
)
if not exist "%EXE%" (
    echo [ERROR] Missing binary: %EXE%
    echo.
    echo Build it first:
    echo   build-server.bat
    set "RC=1"
    goto done
)

REM No port pre-check here on purpose. The listening port belongs to
REM start-plugin-server.ps1; duplicating it in this wrapper means the two drift
REM apart, and a stale copy produces a false "port busy" that blocks a launch
REM which would have worked. Instead the launcher runs, and if it fails the
REM message below points at the likely cause. The server itself already
REM reports a bind failure precisely.

echo Starting Agent2API gateway...
echo.
REM Full path to powershell.exe: the caller's PATH may hold a different one.
REM
REM Arguments are assembled with ordinary %VAR% expansion and handed to a child
REM cmd through a single quoted string. Delayed expansion cannot be used here:
REM inside !...! the embedded quotes of a value get stripped, so a path with
REM spaces arrives as several arguments. %VAR% keeps the quotes intact.
set "PARGS="
if %N% GEQ 1 set PARGS=%PARGS% "%A1%"
if %N% GEQ 2 set PARGS=%PARGS% "%A2%"
if %N% GEQ 3 set PARGS=%PARGS% "%A3%"
if %N% GEQ 4 set PARGS=%PARGS% "%A4%"
if %N% GEQ 5 set PARGS=%PARGS% "%A5%"
if %N% GEQ 6 set PARGS=%PARGS% "%A6%"
if %N% GEQ 7 set PARGS=%PARGS% "%A7%"
if %N% GEQ 8 set PARGS=%PARGS% "%A8%"
if %N% GEQ 9 set PARGS=%PARGS% "%A9%"

set "RC=0"
call :run
goto after_run

:run
"%SYSTEMROOT%\System32\WindowsPowerShell\v1.0\powershell.exe" -NoProfile -ExecutionPolicy Bypass -File "%PS1%" %PARGS%
set "RC=%ERRORLEVEL%"
exit /b %RC%

:after_run
if not "%RC%"=="0" (
    echo.
    echo [ERROR] start-plugin-server.ps1 exited with code %RC%.
    echo.
    echo Common causes:
    echo   - the port is already taken, so another agent2api-server is running
    echo   - AGENT2API_ADMIN_PASSWORD is unset and the panel refused to start
    echo   - the data directory is not writable
    echo The exact reason is in the lines above.
)
goto done

:done
if "%DO_PAUSE%"=="1" pause
endlocal & exit /b %RC%
