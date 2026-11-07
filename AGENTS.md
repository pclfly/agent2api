# 本地开发约束：仅 Server 版

本仓库只开发、调试、测试和编译 `agent2api-server`。除非用户明确要求恢复其他版本，否则跳过 Tauri 桌面版、手机端、安装包、macOS 应用和多平台构建。本文件约束本地日常工作；`release-flow` Skill 中的完整发版流程只约束发版协作本身，不表示已获准构建其他版本。

## 构建与验证

- Rust 命令显式选择 `-p agent2api-server`；发布构建同时选择 `--bin agent2api-server`。workspace 没设 `default-members`，根包是桌面壳 `workbuddy-proxy-desktop`，裸跑 `cargo build` / `cargo check` 会连带编译 Tauri 依赖树。禁止日常执行 `cargo ... --workspace`、`cargo ... --all`、选择 `workbuddy-proxy-desktop`、`tauri dev/build` 或 `npm run tauri:*`。不要为了验证 server 顺带编译桌面壳。
- `build-server.bat` 只构建 release server，不生成安装包：默认带 `--locked`（`Cargo.lock` 对不上会直接失败；故意升依赖后用 `--nolock` 跑一次，锁文件单独提交）；`--check` 只类型检查 server，`--test` 只跑 server 单测。脚本不会自动停止正在运行的服务。
- 所有本地 Rust 产物统一放在仓库根目录 `target/`。仓库根与 `desktop-tauri/src-tauri/` 各有一份 `.cargo/config.toml`，`target-dir` 都钉到仓库根 —— 从哪里跑 cargo 都落同一处。不要另设 `CARGO_TARGET_DIR` 或 `--target-dir` 生成第二套缓存；`desktop-tauri/src-tauri/target/` 属于历史遗留，清理后不应再出现。
- 默认关闭增量编译缓存（根 `.cargo/config.toml` 的 `incremental = false`），保留默认调试符号以支持断点调试。需要增量编译提速时先取得用户同意，不要静默用环境变量覆盖配置。
- 优先运行 `cargo check` 和受影响的 server 测试；只有交付二进制时才做 release 构建，不要每次同时构建 debug 和 release。
- server 网页管理面板不是桌面版：修改网页时可以按需运行 `ui-islands`、`ui-kit`、`i18n` 的相关测试或构建，禁止因此触发 Tauri 打包。

仓库根目录常用命令：

```bash
cargo check --manifest-path desktop-tauri/src-tauri/Cargo.toml -p agent2api-server --locked
cargo test --manifest-path desktop-tauri/src-tauri/Cargo.toml -p agent2api-server --locked
cargo build --manifest-path desktop-tauri/src-tauri/Cargo.toml -p agent2api-server --bin agent2api-server --release --locked
```

运行现有 server 使用 `start-plugin-server.bat` 或 `start-plugin-server.ps1`；调试构建输出为 `target/debug/agent2api-server.exe`，release 输出为 `target/release/agent2api-server.exe`。启动调试实例时使用独立测试数据目录和空闲端口，不得抢占正在运行的服务或复用生产数据库。

## 保留与清理边界

- 不得整体删除 `desktop-tauri/`：其中 `src-tauri/server/` 是 server 源码，`ui/` 是共享网页资源。
- 保留全部现有未提交修改、Git 历史、运行数据和正在使用的 server EXE。
- 本约束不会自动清理旧缓存。删除缓存、安装包或重复产物须单独确认；不得直接 `cargo clean` 删除正在使用的 server EXE。
- 不自动修改上游发布工作流，不自动提交、推送、打 tag 或发布。

## 个人 Fork 与官方同步（本工作区优先）

- 工作区：`D:\Ai\api\agent2api-dev\Agent2API`；开发分支：`custom/agent2api-server`，以本地代码为基准。
- `origin` 为个人 Fork `https://github.com/pclfly/agent2api.git`；`upstream` 为官方 `https://github.com/aimod-cc/Agent2API.git`；默认推送目标只能是 `origin`。
- 只有用户在当前任务中明确下达「同步 agent2api 官方更新」才执行官方同步；引用口令、讨论流程、迁移目录不构成授权。平时不主动 fetch/pull/merge/commit/push。
- 同步使用用户级 `sync-upstream-agent2api` Skill：先检查工作区/分支/远程/提交关系，检查敏感文件并保存本地改动；有未处理代码时先提交并推送个人 custom 分支作为合并前备份，失败即停止。随后从当前 custom 分支建立不覆盖的 `backup/custom-baseline-YYYYMMDD`（重名加序号），再获取并 merge `upstream/main`。
- 冲突保留自定义功能和本地改动并吸收官方更新；不能安全判断时暂停询问。不 rebase、不改写历史、不强推，不用整文件 ours/theirs 代替核对。
- 运行本文件规定的 server 测试及受影响网页验证；测试失败不推送合并结果，除非用户另行明确要求。通过后仅推 `origin/custom/agent2api-server`，最终报告回滚点/hash、合并提交、冲突、测试与两阶段推送结果。
- 本个人维护流程不套用下方 release-flow 的 beta/main 日常落脚点或历史改写示例；不授权推 upstream、main/beta、tag 或发 Release。发版仍须另行确认。
- 默认同步只涵盖本仓；`D:\Ai\api\agent2api-dev\Agent2Providers` 须用户明确纳入。旧工作区 `D:\Ai\api\agent2api` 和迁移前目录不是备用同步目标，不修改其分支。
- 运行目录固定保留 `D:\Ai\api\agent2api-plugin-data`；不提交其数据库、密钥、凭据或插件运行状态。迁移保留的旧源码副本只作参考，不在新旧目录同时开发。
- 2026-11-07 迁移采用复制并校验：原有未提交改动保留，未提交/推送迁移代码，未执行官方同步；不得把示例测试数字视为本仓已验证基线。

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

其余细则（分支落脚点、署名与 AI 落款、六处版本号、正式版 / beta 完整流程、验收清单、已知坑与排查）见上述 Skill。

## 反代提供商问题排查

收到反代提供商相关的问题反馈时，**优先使用网络搜索查找已有解决方案**，重点参考同类项目的 Issue、PR、修复记录和适配实现，了解其他项目是否遇到过相同问题、如何解决。**不要默认从零自行排查**：先核对已有方案与本项目的适用性，优先借鉴可靠的解决思路；未找到可靠方案、现有方案不适用或仍有未解决的问题时，再开展针对性的本地排查。

## 文档落点

安装 / 部署 / 升级 / 排错说明只在 `docs/install.md`（另有 5 份译文 `docs/install.{en,zh-Hant,ja,ko,pt-BR}.md`，六份一一对应）里维护；各语言 README 只留「快速开始」速查表 + 一个指向安装文档的链接，**不要再把安装细节写回 README**。产物名、端口、环境变量、安装脚本参数有改动时，六份一起改。

**README 语言落点：根 `README.md` 是英文**（GitHub 默认展示），简体中文在 `docs/README.zh-CN.md`，其余四份为 `docs/README.{zh-Hant,ja,ko,pt-BR}.md`。中文仍是文案源语言，改支持矩阵等内容时六份同步，别把中文写回根目录。
