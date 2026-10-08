@echo off
rem ============================================================
rem  Agent2API 桌面端安装包 - 一键构建脚本
rem
rem  用法：
rem    build-installer.bat              双击运行，或命令行直接执行
rem    build-installer.bat clean        先 cargo clean，再全量构建
rem    build-installer.bat -nopause     构建结束不停留（供脚本/CI 调用）
rem
rem  产物：target\release\bundle\nsis\Agent2API_<版本>_x64-setup.exe
rem  该 exe 可直接拷到其他 Windows 电脑安装，目标机无需 Node / Rust。
rem
rem  ── 本文件的编码是 GBK（cp936），且刻意不调用 chcp ──────────
rem  中文 Windows 的控制台默认代码页就是 936，文件编码与它一致时
rem  cmd 按字节顺序逐行解析，不会错位。反过来，如果文件存成 UTF-8
rem  再在脚本里 chcp 65001，cmd 会先用旧代码页算好文件偏移、切换后
rem  按新代码页继续读 —— 行边界会错开，症状是「注释行被当成命令
rem  执行」「echo 的正文被吃掉一段」。所以这里不要加 chcp。
rem  cd D:\Ai\api\agent2api\desktop-tauri\src-tauri
rem  cargo build --release -p agent2api-server
rem ============================================================

setlocal EnableExtensions EnableDelayedExpansion
cd /d "%~dp0"

set "ROOT=%~dp0"
set "DESKTOP=%ROOT%desktop-tauri"
set "NSIS_DIR=%ROOT%target\release\bundle\nsis"
set "DO_CLEAN=0"
set "DO_PAUSE=1"

:parse_args
if "%~1"=="" goto args_done
if /I "%~1"=="clean" (
    set "DO_CLEAN=1"
    shift
    goto parse_args
)
if /I "%~1"=="-nopause" (
    set "DO_PAUSE=0"
    shift
    goto parse_args
)
echo [错误] 未知参数: %~1
echo 用法: %~nx0 [clean] [-nopause]
goto fail

:args_done
echo.
echo ============================================================
echo   Agent2API 桌面端安装包 一键构建
echo ============================================================
echo   项目根目录 : %ROOT%
echo.

rem ── 0. 前置工具检查 ─────────────────────────────────────────
rem rustup 把 %USERPROFILE%\.cargo\bin 写进「用户级 PATH」，而双击启动的
rem cmd 继承的是资源管理器的环境块：资源管理器若在装 rustup 之前就启动了，
rem 它那份环境里没有 .cargo\bin，where cargo 就会落空。这里主动补一次，
rem 免得「明明装了 Rust 却被判成缺少依赖」。
if not exist "%USERPROFILE%\.cargo\bin\cargo.exe" goto check_tools
where cargo >nul 2>nul && goto check_tools
set "PATH=%USERPROFILE%\.cargo\bin;%PATH%"
echo   提示       : 已把 %USERPROFILE%\.cargo\bin 补进本次运行的 PATH
echo.

:check_tools
where node  >nul 2>nul || goto no_node
where npm   >nul 2>nul || goto no_node
where cargo >nul 2>nul || goto no_rust
where rustc >nul 2>nul || goto no_rust

for /f "tokens=*" %%v in ('node --version')  do set "NODEVER=%%v"
for /f "tokens=*" %%v in ('cargo --version') do set "CARGOVER=%%v"
echo   工具链     : Node !NODEVER! / !CARGOVER!
echo.

rem ── 1. 桌面端依赖（@tauri-apps/cli 等）─────────────────────
if exist "%DESKTOP%\node_modules\.bin\tauri.cmd" (
    echo [1/3] 桌面端依赖已就绪，跳过安装
) else (
    echo [1/3] 安装桌面端依赖，首次执行需要联网...
    call npm --prefix "%DESKTOP%" install
    if errorlevel 1 goto fail
)

rem ── 2. 可选：清理编译缓存 ─────────────────────────────────
if "%DO_CLEAN%"=="1" goto do_clean
echo [2/3] 增量编译。需要全量重建时执行: build-installer.bat clean
goto build_step

:do_clean
echo [2/3] 清理编译缓存，cargo clean，下一次为全量编译...
pushd "%DESKTOP%\src-tauri"
cargo clean
set "CLEAN_RC=%ERRORLEVEL%"
popd
if not "%CLEAN_RC%"=="0" goto fail

:build_step
rem ── 3. 构建安装包 ─────────────────────────────────────────
echo [3/3] 构建安装包...
echo       首次全量编译约 5-10 分钟，请勿关闭本窗口。
echo.
call npm --prefix "%DESKTOP%" run build
if errorlevel 1 goto fail

rem ── 4. 汇报产物 ───────────────────────────────────────────
set "OUT="
for %%f in ("%NSIS_DIR%\*.exe") do set "OUT=%%~ff"
if not defined OUT (
    echo.
    echo [错误] 构建命令已返回成功，但没有找到安装包：
    echo        %NSIS_DIR%\*.exe
    goto fail
)

echo.
echo ============================================================
echo   构建完成
echo ============================================================
for %%f in ("%NSIS_DIR%\*.exe") do echo   安装包 : %%~ff  [%%~zf 字节]
echo.
echo   把这个 exe 拷到其他 Windows 电脑双击安装即可：
echo     · 目标机不需要 Node / Rust / 任何运行时
echo     · 安装需管理员授权，默认装到 C:\Program Files\Agent2API
echo     · 目标机若没有 WebView2，安装器会自动联网下载
echo.

if "%DO_PAUSE%"=="0" goto done
start "" explorer /select,"%OUT%"
echo   按任意键关闭本窗口。
pause

:done
endlocal
exit /b 0

:no_node
echo [缺少依赖] 未找到 Node.js，构建脚本需要 Node 18.17 或更高版本
echo            下载地址: https://nodejs.org/
goto fail

:no_rust
echo [缺少依赖] 未找到 Rust 工具链，需要 rustc 1.88 或更高版本
echo            安装地址: https://rustup.rs/
echo            Windows 上还需要 MSVC C++ 生成工具:
echo            https://visualstudio.microsoft.com/visual-cpp-build-tools/
echo            装完 rustup 后请重新登录一次，或重启资源管理器，
echo            否则双击启动的窗口可能读不到新的 PATH。
goto fail

:fail
echo.
echo ------------------------------------------------------------
echo   构建失败：请查看上方错误信息。
echo ------------------------------------------------------------
if "%DO_PAUSE%"=="1" pause
endlocal
exit /b 1
