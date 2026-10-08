@echo off
rem ============================================================
rem  Agent2API 局域网访问开关
rem
rem  为什么需要它：安装包不会替你设监听地址 —— 桌面端默认只听
rem  127.0.0.1（这条默认值是有意的，见 gateway.rs 里 bind_host 的
rem  说明）。装完安装包直接启动，其他电脑必然连不上。
rem
rem  本脚本做四件事：
rem    1) 结束正在运行的实例（它此刻多半只听着 127.0.0.1）
rem    2) 把 AGENT2API_HOST=0.0.0.0 写成用户级环境变量
rem    3) 加一条入站防火墙规则，放行该程序的 TCP 入站
rem    4) 用新设置重新启动，并打印可用的访问地址
rem
rem  用法：右键「以管理员身份运行」
rem        enable-lan-access.bat "D:\自定义路径\workbuddy-proxy-desktop.exe"
rem
rem  编码：GBK，且刻意不调用 chcp（原因见 build-installer.bat 头部）
rem ============================================================

setlocal EnableExtensions EnableDelayedExpansion
cd /d "%~dp0"

set "EXE=%~1"
if "%EXE%"=="" set "EXE=%ProgramFiles%\Agent2API\workbuddy-proxy-desktop.exe"
set "PORT=3065"
set "RULENAME=Agent2API LAN"

rem net session 需要管理员权限，用它做提权检测
net session >nul 2>nul
if errorlevel 1 goto need_admin

echo.
echo ============================================================
echo   Agent2API 局域网访问开关
echo ============================================================
echo   程序 : %EXE%
echo   端口 : %PORT%
echo.

if not exist "%EXE%" goto no_install

echo [1/4] 结束正在运行的实例...
taskkill /IM workbuddy-proxy-desktop.exe /F >nul 2>nul
timeout /t 2 /nobreak >nul

echo [2/4] 写入用户级环境变量 AGENT2API_HOST=0.0.0.0 ...
setx AGENT2API_HOST 0.0.0.0 >nul
if errorlevel 1 goto fail

echo [3/4] 添加入站防火墙规则...
netsh advfirewall firewall delete rule name="%RULENAME%" >nul 2>nul
netsh advfirewall firewall add rule name="%RULENAME%" dir=in action=allow program="%EXE%" protocol=TCP profile=any enable=yes >nul
if errorlevel 1 goto fail

echo [4/4] 用新设置重新启动...
set "AGENT2API_HOST=0.0.0.0"
start "" "%EXE%"
timeout /t 8 /nobreak >nul

echo.
echo ============================================================
echo   当前监听
echo ============================================================
netstat -ano | findstr ":%PORT%" | findstr "LISTENING"
echo.
echo   其他电脑可以用的地址：
powershell -NoProfile -Command "Get-NetIPAddress -AddressFamily IPv4 | Where-Object { $_.IPAddress -match '^(192\.168\.|10\.|172\.(1[6-9]|2[0-9]|3[01])\.)' } | ForEach-Object { '     http://' + $_.IPAddress + ':%PORT%/v1' }"
echo.
echo   收尾提示：
echo     · 上面若显示 127.0.0.1:%PORT% 而不是 0.0.0.0:%PORT%，说明没生效
echo     · 环境变量已写入，但「开始菜单」启动的窗口要注销或重启后才读得到；
echo       想立刻用，就用本脚本启动，或直接双击安装目录里的程序
echo     · 到别的电脑上先确认端口通不通（PowerShell）：
echo         Test-NetConnection 上面的IP -Port %PORT%
echo     · 还没建 API Key 的话请立刻在「网关 Key」页建一把：没配 Key 时
echo       /api/* 与 /v1/* 对同网段完全开放，账号和额度都在裸奔
echo.
pause
endlocal
exit /b 0

:need_admin
echo [错误] 需要管理员权限：改环境变量与改防火墙规则都要提权。
echo        请关掉本窗口，右键本文件，选「以管理员身份运行」。
goto fail

:no_install
echo [错误] 没找到桌面端程序：
echo        %EXE%
echo        若装到了别的位置，把完整路径当参数传进来，例如：
echo        enable-lan-access.bat "D:\Program Files\Agent2API\workbuddy-proxy-desktop.exe"
goto fail

:fail
echo.
echo ------------------------------------------------------------
echo   未能完成，请查看上方错误信息。
echo ------------------------------------------------------------
pause
endlocal
exit /b 1
