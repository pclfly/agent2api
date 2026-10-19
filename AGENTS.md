# 本地开发约束：仅 Server 版

本工作区只开发、调试、测试和编译 `agent2api-server`。除非用户明确要求恢复其他版本，否则跳过 Tauri 桌面版、安装包、macOS 应用和多平台构建。本文件约束本地日常工作；`agent.md` 中的上游完整发版流程不表示已获准构建其他版本。

## 构建与验证

- Rust 命令显式选择 `-p agent2api-server`；发布构建同时选择 `--bin agent2api-server`。workspace 的 `default-members` 也只选择 server。
- 禁止日常执行 `cargo ... --workspace`、`cargo ... --all`、选择 `workbuddy-proxy-desktop`、`tauri dev/build` 或 `npm run tauri:*`。不要为了验证 server 顺带编译桌面壳。
- `build-installer.bat` 保留历史文件名，但已改为仅构建 release server，不再生成安装包；`--check -nopause` 只检查 server，`-nopause` 构建 server。旧 `clean` 参数已禁用，脚本不会自动停止正在运行的服务。
- 所有本地 Rust 产物统一放在仓库根目录 `target/`。从仓库根或其子目录执行 Cargo，使项目 `.cargo/config.toml` 生效；不要另设 `CARGO_TARGET_DIR` 或 `--target-dir` 生成第二套缓存。
- 默认关闭增量编译缓存，保留默认调试符号以支持断点调试。需要增量编译提速时先取得用户同意，不要静默用环境变量覆盖配置。
- 优先运行 `cargo check` 和受影响的 server 测试；只有交付二进制时才做 release 构建，不要每次同时构建 debug 和 release。
- server 网页管理面板不是桌面版：修改网页时可以按需运行 `ui-islands`、`ui-kit`、`i18n` 的相关测试或构建，禁止因此触发 Tauri 打包。

仓库根目录常用命令：

```bash
cargo check --manifest-path desktop-tauri/src-tauri/Cargo.toml -p agent2api-server --locked
cargo test --manifest-path desktop-tauri/src-tauri/Cargo.toml -p agent2api-server --locked
cargo build --manifest-path desktop-tauri/src-tauri/Cargo.toml -p agent2api-server --bin agent2api-server --release --locked
```

运行现有 server 使用 `run-server.bat` 或 `run-server.ps1`；调试构建输出为 `target/debug/agent2api-server.exe`，release 输出为 `target/release/agent2api-server.exe`。启动调试实例时使用独立测试数据目录和空闲端口，不得抢占正在运行的服务或复用生产数据库。

## 保留与清理边界

- 不得整体删除 `desktop-tauri/`：其中 `src-tauri/server/` 是 server 源码，`ui/` 是共享网页资源。
- 保留全部现有未提交修改、Git 历史、运行数据和正在使用的 server EXE。
- 本约束不会自动清理旧缓存。删除缓存、安装包或重复产物须单独确认；不得直接 `cargo clean` 删除正在使用的 server EXE。
- 不自动修改上游发布工作流，不自动提交、推送、打 tag 或发布。

## 上游协作入口（不得覆盖以上本地约束）

# AGENTS.md

> **涉及分支、发版、版本号的操作，一律按 Skill `release-flow` 执行：**
> **`.agents/skills/release-flow/SKILL.md`** —— 动手前先读它，细则一律以该文件为准，本文件只作入口指路。
>
> 覆盖范围：切分支 / 给 PR 选 base / 合并 PR / 回复 Issue、PR / 改版本号 / 写更新日志 /
> 打 tag / 挂 GitHub Release / 发 beta 预发布版 / 发版验收 / 排查发版失败。

## 两条红线（先问人，不得自行推进）

1. **发版**：改版本号、推 `main` / `beta`、打 tag、挂 Release 之前，先把版本号与「这次要发什么」讲清楚，等用户确认；**未经确认不得推任何分支或 tag** —— tag 一旦推出去就会触发构建、产出公开的安装包与镜像，收不回来。
2. **更新日志**：写好的日志（= 发布提交信息 = Release 正文）必须先给用户逐条过目；按反馈改完**再确认一次**才能提交。

其余细则（分支落脚点、署名与 AI 落款、五处版本号、正式版 / beta 完整流程、验收清单、已知坑与排查）见上述 Skill。

## 文档落点

安装 / 部署 / 升级 / 排错说明只在 `docs/install.md`（另有 5 份译文 `docs/install.{en,zh-Hant,ja,ko,pt-BR}.md`，六份一一对应）里维护；各语言 README 只留「快速开始」速查表 + 一个指向安装文档的链接，**不要再把安装细节写回 README**。产物名、端口、环境变量、安装脚本参数有改动时，六份一起改。
