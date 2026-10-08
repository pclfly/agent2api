# Agent2API headless 网关 —— 本机启动脚本
#
# 用法：
#   .\run-server.ps1                              # 127.0.0.1:3065，数据落 .\data
#   .\run-server.ps1 -BindHost 0.0.0.0            # 允许局域网访问（面板同时暴露）
#   .\run-server.ps1 -Port 8080 -VerboseLog       # 换端口 + debug 日志
#
# 首次启动后打开 http://127.0.0.1:3065/ 注册管理员账号；
# 登录后在「网关 Key」页创建 API Key，/v1/* 在此之前 fail-closed。

param(
    [string]$BindHost = "127.0.0.1",
    [int]$Port = 3065,
    [switch]$VerboseLog
)

$ErrorActionPreference = "Stop"
$root = $PSScriptRoot
$exe = Join-Path $root "target\release\agent2api-server.exe"

if (-not (Test-Path $exe)) {
    throw "未找到 $exe —— 请先在 desktop-tauri\src-tauri 下执行：cargo build --release -p agent2api-server"
}

# 数据目录（SQLite 库 / config.json / 日志）落在项目根的 data\ 下，已被 .gitignore 忽略
$env:AGENT2API_PROXY_HOME = Join-Path $root "data"
# 面板静态资源
$env:AGENT2API_UI_DIR = Join-Path $root "desktop-tauri\ui"
$env:AGENT2API_HOST = $BindHost
$env:AGENT2API_PROXY_PORT = "$Port"
if ($VerboseLog) { $env:AGENT2API_VERBOSE = "1" }

New-Item -ItemType Directory -Force -Path $env:AGENT2API_PROXY_HOME | Out-Null

$indexHtml = Join-Path $env:AGENT2API_UI_DIR "index.html"
if (-not (Test-Path $indexHtml)) {
    Write-Warning "面板界面缺失：$indexHtml 不存在，管理页面会返回 404（/v1 网关不受影响）"
}

Write-Host "面板  http://${BindHost}:${Port}/"
Write-Host "网关  http://${BindHost}:${Port}/v1"
Write-Host "数据  $env:AGENT2API_PROXY_HOME"
Write-Host "界面  $env:AGENT2API_UI_DIR"
Write-Host ""

& $exe
