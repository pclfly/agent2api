# Agent2API v3.4.0 (plugin edition) - headless server launcher
# 用法：powershell -ExecutionPolicy Bypass -File .\start-plugin-server.ps1
#
# 数据目录默认使用 D:\Ai\api\agent2api-plugin-data，独立于源码目录。
# 要放到别处，按优先级从高到低任选一种：
#   .\start-plugin-server.ps1 -DataHome 'E:\agent2api-data'
#   $env:AGENT2API_PROXY_HOME = 'E:\agent2api-data'
#
# 其他开关：
#   -DryRun       只打印解析后的配置，不启动服务
#   -MigrateData  把旧数据目录里的库复制到新目录（原目录保留，不删不改）
#
# 编码：本文件必须保存为 UTF-8 with BOM + CRLF。Windows PowerShell 5.1 读无
# BOM 的 UTF-8 会按 ANSI 解码，下面所有中文提示都会变成乱码。
#
# 端口、面板账号也集中在下面，按需修改。

[CmdletBinding()]
param(
    [string]$DataHome,
    [switch]$MigrateData,
    [switch]$DryRun
)

$ErrorActionPreference = 'Stop'

$Root  = Split-Path -Parent $MyInvocation.MyCommand.Path
# 产物在仓库根的 target\release\（desktop-tauri\src-tauri\.cargo\config.toml
# 把 target-dir 固定为 "../../target"）—— 不是 src-tauri\target\。写成后者会让脚本
# 一直启动一个构建流程早已不再更新的旧二进制，症状是「改了代码、行为没变」。
$Exe   = Join-Path $Root 'target\release\agent2api-server.exe'
$UiDir = Join-Path $Root 'desktop-tauri\ui'
$Port  = 3065

# 运行数据始终与源码分开；迁移源码不改变这个目录。
$DefaultDataHome = 'D:\Ai\api\agent2api-plugin-data'
$LegacyDataHome = $DefaultDataHome
$DbName = 'agent2api.db'

# 数据目录解析：参数 > 环境变量 > 既有外部运行目录
if ($DataHome) {
    $HomeSource = '参数 -DataHome'
} elseif ($env:AGENT2API_PROXY_HOME) {
    $DataHome   = $env:AGENT2API_PROXY_HOME
    $HomeSource = '环境变量 AGENT2API_PROXY_HOME'
} else {
    $DataHome   = $DefaultDataHome
    $HomeSource = '默认（外部运行目录）'
}

if (-not (Test-Path $Exe)) {
    throw "未找到 server 二进制：$Exe`n先执行：.\build-server.bat"
}

$LegacyDb = Join-Path $LegacyDataHome $DbName

if ($DryRun) {
    $dbState = if (Test-Path (Join-Path $DataHome $DbName)) { '是' } else { '否（首次启动会新建）' }
    Write-Host '（-DryRun 不启动服务，不创建或复制数据）'
    Write-Host "二进制        ->  $Exe"
    Write-Host "监听地址      ->  http://127.0.0.1:$Port"
    Write-Host "数据目录      ->  $DataHome"
    Write-Host "  解析来源    ->  $HomeSource"
    Write-Host "  已有数据库  ->  $dbState"
    Write-Host "UI 目录       ->  $UiDir"
    Write-Host "插件目录      ->  $(Join-Path $DataHome 'plugins')"
    return
}

if ($MigrateData) {
    if ([IO.Path]::GetFullPath($DataHome).TrimEnd('\') -eq [IO.Path]::GetFullPath($LegacyDataHome).TrimEnd('\')) {
        throw '源数据目录和目标相同，无需迁移；如需复制，请用 -DataHome 指定独立目标。'
    }
    if (-not (Test-Path $LegacyDb)) {
        throw "没有可迁移的旧数据：$LegacyDb 不存在"
    }
    $running = Get-Process -Name 'agent2api-server' -ErrorAction SilentlyContinue
    if ($running) {
        $pids = ($running | ForEach-Object { $_.Id }) -join ', '
        throw "检测到 agent2api-server 正在运行（PID $pids）。`n复制运行中的数据库会得到不一致的快照，先停掉服务再迁移。"
    }
    if (-not (Test-Path $DataHome)) { New-Item -ItemType Directory -Path $DataHome | Out-Null }
    Copy-Item -Path (Join-Path $LegacyDataHome '*') -Destination $DataHome -Recurse -Force
    Write-Host "已把旧数据复制到新目录。原目录 $LegacyDataHome 保留，确认无误后自行删除。"
}

if (-not (Test-Path $DataHome)) { New-Item -ItemType Directory -Path $DataHome | Out-Null }

$env:AGENT2API_PROXY_HOME      = $DataHome
$env:AGENT2API_PROXY_PORT      = "$Port"
$env:AGENT2API_HOST            = '127.0.0.1'
$env:AGENT2API_UI_DIR          = $UiDir
$env:AGENT2API_CAPTCHA_ENABLED = '0'   # 本机回环部署；对外暴露时改回 1
if (-not $env:AGENT2API_ADMIN_USER)     { $env:AGENT2API_ADMIN_USER     = 'admin' }
if (-not $env:AGENT2API_ADMIN_PASSWORD) { Write-Warning 'AGENT2API_ADMIN_PASSWORD 未设置，首次启动将无法登录面板' }

Write-Host "Agent2API server  ->  http://127.0.0.1:$Port"
Write-Host "数据目录          ->  $DataHome"
Write-Host "插件目录          ->  $(Join-Path $DataHome 'plugins')"
& $Exe
# 把子进程退出码传出去。不写这一行时脚本自身始终以 0 结束，于是服务端
# 绑定端口失败、配置报错这类情况在调用方看来都是「成功」，start-plugin-server.bat
# 也会跟着返回 0。
exit $LASTEXITCODE
