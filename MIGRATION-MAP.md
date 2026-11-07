# 本地功能 → 新版落点 对照表

- 旧版本：`D:\Ai\api\agent2api`，分支 `custom/agent2api`，HEAD `ed035935`，基线官方 v3.1.0（`072e3c71`）+ 13 个本地提交 + 189 个未暂存修改 + 21 个未跟踪文件
- 新版本：`D:\Ai\api\agent2api-plugin`，tag `v3.4.0`（`31bc7bdb`），插件 SDK 锁 `198b5051`
- 插件宿主：`agent2api-providers` v3.4.0（32 家）、`agent2api-web-providers` v3.4.0（2 家）
- 结论口径：**定制宿主 + 自定义插件**。宿主差异继续打补丁维护，能进插件的进插件。

## 迁移形态代号

| 代号 | 含义 | 落地位置 |
|---|---|---|
| **P** | 迁为 provider 插件 | `Agent2Providers` 新 provider 或改现有 provider |
| **H** | 保留为宿主补丁（core） | 新版 `server/src/server/core/…` 打差异 |
| **U** | 迁到宿主管理界面 | `desktop-tauri/ui`、`ui-islands/src/islands` |
| **D** | 数据迁移/结构桥接 | `db/schema.rs` + 一次性迁移脚本 |
| **X** | 新版已删除对应底座，需重建 | 先从旧版取实现，再按新架构重写 |
| **✔** | 新版已覆盖，只需验收 | 不迁移，跑回归 |

---

# 迁移状态总览

> 状态口径：**✅ 已同步并实测** / **🟡 部分同步（缺口写明）** / **❌ 未同步（含「判定不做」，说明列写明理由）** / **⚪ 新版自带，无需迁移** / **❓ 未验证**。
> 「实测」= 真跑过接口或转发，不是只编译通过。

## 状态 A｜按批次

| 批次 | 内容 | 状态 | 实测证据 |
|---|---|---|---|
| B0 | 数据库桥接到 schema v23 | ✅ | 16 步桥接；旧库 76 账号 / 19,162 请求完好；`integrity_check=ok` |
| B1 | 代理核心（两跳 / 严格出口 / 订阅节点） | ✅ | 7 个账号出口恢复；19105→`216.36.105.42` 实网核对 |
| B2 | 积分链路（实报 + 估算 + 报表） | ✅ | 实报 `0.30882467` 逐位一致；估算 `estimate` 标记正确 |
| B3 | Bedrock / Vertex / 提示词范围 | ✅ | AWS 403 `invalid token`（签名正确）；Google 400 `invalid key`（信封正确） |
| B4 | 订阅与内核管理 | ✅ | 内核自动拉起；37→92 监听器；订阅刷新 0→47 节点 |
| B5 | MiniMax 插件 | ✅ | 真实转发 HTTP 200，`reasoning_content` 证明 adaptive 思考生效；签到/积分/目录/续期四条维护链路补齐（见 #5） |
| B6 | 界面四块 + 多语言 | ✅ | 前端构建通过；积分三件套 5 组；23/29 选项带模型目录 |
| B7 | 订阅管理界面 | ✅ | 订阅增删改查全通；内核停止/重启/`halted` 语义正确 |
| 额外 | 出口就绪检查（见下表） | ✅ | 杀内核→2 秒自动拉起；运行期 502 拒绝而非直连 |

## 状态 B｜逐项状态（对应下面 62 条编号）

### 提供商（1–11）

| # | 功能 | 状态 | 说明 |
|---|---|---|---|
| 1 | LobsterAI 全套 | ⚪ | 官方插件已有（chat/checkin/credentials/oauth/usage/models/version） |
| 2 | LobsterAI 出站指纹 | ❌ | 未做。改官方插件需长期 fork 维护，判定不做（见 B5 报告第七节） |
| 3 | LobsterAI 续期/签到增强 | ❌ | 同上 |
| 4 | Raccoon 三层指纹头 | ❌ | 同上。插件现仅 `X-Org-Code`，本地 20 项指纹未迁 |
| 5 | MiniMax Code | ✅ | 转发 + 续期 + 签到 + 积分 + 远程目录 + 设备码登录全部实现。实测：签到 `今日已签到`（矩阵认证）、积分 `available=20568.52` / 31 个包、目录刷新 `count=2`、转发 `两跳` |
| 6 | QwenWork 国内版 | ⚪ | 官方插件已实现（`qoder` 模块 `QwenworkCn`）；本地 `daily_gift` 余额对比未逐项比对 |
| 7 | QwenWork 国际版 | ❌ | 官方 `SUSPENDED_IDS` 停用（上游回调白名单限制），未恢复 |
| 8 | WorkBuddy 网页每日任务 | ⚪ | 官方插件 `workbuddy/growth/` 已有；未逐项比对 |
| 9 | WorkBuddy 出站身份按钮 | ❌ | 未移植，也未验证新版账号页是否仍需要它 |
| 10 | 会话粘性 | ⚪ | 新版 `route_sticky` 已重写并增强（`sticky_list` / `sticky_clear` / TTL） |
| 11 | 地区收窄 `restrict_edition_candidates` | ❌ | 新版 0 引用，本地 13 处。未移植 |

### 协议与提示词（12–20）

| # | 功能 | 状态 | 说明 |
|---|---|---|---|
| 12 | AWS Bedrock Converse | ✅ | 2593 行移植；SigV4 + Event Stream；假凭证获得 AWS 403 |
| 13 | Vertex Gemini | ✅ | 996+171 行移植；信封 + thoughtSignature + usage |
| 14 | 自定义协议扩展 | ✅ | `PROTOCOLS` 5 项；前端下拉同步 5 项 |
| 15 | 插件能否承载这两家 | ✅ 已判定 | 不能（`UpstreamKind` 装不下二进制分帧），保留宿主 |
| 16 | Anthropic `cache_creation_input_tokens` 透传 | ✅ | `usage_to_anthropic` 补写入项并从普通输入里减掉（Bedrock 路径实测 512 vs 514 的差归零）；没有该字段的旧上游保持原形状 |
| 17 | 提示词按模型范围 | ✅ | `parse_model_scope` + `for_model`；实测 `model=Qwen3.8-Max` 命中注入 |
| 18 | 提示词「适用模型」选择器 | ✅ | 弹窗 + 状态层 + 后端 `providerOptions.models`（修前 0 项、修后 23/29） |
| 19 | MiniMax 提示缓存断点 | ❌ | 未做（属 MiniMax 插件增强，随第 5 项） |
| 20 | 图片压缩 | ⚪ | 新版自带 `image_compress` |

### 积分（21–27）

| # | 功能 | 状态 | 说明 |
|---|---|---|---|
| 21 | `credit` 落库 | ✅ | 补 `entry.credit = snapshot.credit`；实测与上游返回值逐位一致 |
| 22 | `credit_source` | ✅ | schema **v24**（带列存在判断，兼容本地历史库）；`reported`/`estimate` 两态 |
| 23 | 积分估算器 | ✅ | 305 行移植。**注意**：4 家 `scale` 仍是未标定假设，界面靠 `≈` 区分 |
| 24 | 报表按账号积分聚合 | ✅ | `push_account_credit` + 日报累计；实测 5 组正确 |
| 25 | 前端积分展示 | ✅ | `credit-measurement.ts` + `rankRows` 悬停说明（`≈` / `*` / `—`） |
| 26 | 请求日志「消耗」列 | ✅ | 接口三件套 + 表格列（`creditSource` → `≈` 前缀）+ 报表排行悬停说明 |
| 27 | 保活/测试流量的积分处理 | ❌ | 未验证 `keepalive` 路径在新版是否写 credit |

### 代理（28–39）

| # | 功能 | 状态 | 说明 |
|---|---|---|---|
| 28 | 两跳前置代理 | ✅ | **真实转发实测**：`[front] CONNECT 127.0.0.1:18902` → `[back] CONNECT agent.minimax.cn:443`，返回内容 `两跳`。修好三处断点（见下「28 的三处断点」） |
| 29 | 代理订阅 + 内核 | ✅ | B4 恢复 2210 行；内核自动拉起、端口自动分配 |
| 30 | 订阅导入 | ✅ | 三种来源全通：链接 / 本地文件（`contentBase64` + `fileName`）/ 二维码图片（`imageBase64` → `rqrr` 解码）。界面 `SubscriptionModal` 带来源切换与二维码选择器 |
| 31 | `failureMode`（smart/all_errors） | ✅ | 归一（缺键 = smart、非法值 400）+ `should_rotate_on` 判据。实测三态往返：`smart` / `all_errors` 存得住，`bogus` 返回 400 |
| 32 | 出口冷却持久化 | ✅ | `proxy_rotate/health.rs`（822 行）。**实测重启恢复**：写一条冷却 → 重启 → 日志 `♻️ 已恢复 1 条出口冷却` → API `cooling:1 / failures:2 / remainingMs:271221` |
| 33 | 出口健康探测 | ✅ | `proxy_rotate/probe.rs`（332 行）：`HEAD origin/`、不带凭据、不跟跳转、任何状态码算通、singleflight + 30s/1s 缓存；冷却到期时先探再发。4 个用例覆盖（含「预算耗尽不发探测」） |
| 34 | 出口准入与负载均衡 | ✅ | `proxy_rotate/admission.rs`（466 行）+ 3 个配置键（1..=32 / 0..=1024 / 1..=300）。实测越界值返回 400，合法值落库并回显 |
| 35 | 请求级出口均衡 | ✅ | 准入按 `(origin, 出口标识)` 计数、排队、超时；`Permit` 用 Drop 释放（`?` 早退不漏名额）。4 个用例覆盖限流/排队/释放唤醒/超时 |
| 36 | 429 / `Retry-After` 出口冷却 | ✅ | `retry_after_delay` 认三种形态（秒数 / RFC 2822 / 文案里的恢复时刻），夹到一天；退避取 `max(2^n 秒, Retry-After)`。用例覆盖全部三形态 + 夹逼 |
| 37 | 维护链路严格出口 `provider_route` | ✅ | 模块 277 行 + 明确「无调用点是有意为之」：新版把签到/余额/目录交给插件，宿主侧只剩保活；严格出口由插件从账号记录解析 `proxy` 兑现（模块头已写） |
| 38 | 节点可见性 / 冷却 / 负载面板 | ✅ | 订阅区块：来源徽章、节点数、端口状态（`listening` / `silent`）、未监听提示。冷却/负载按判定不做（见 #31–36） |
| 39 | 新版网络回退 | ⚪ | 新版自带 `network_fallback` |

### 账号与调度（40–46）

| # | 功能 | 状态 | 说明 |
|---|---|---|---|
| 40 | 补签 / 兑换 / 抽奖 | ⚪ | 新版自带 |
| 41 | 猫猫旅行排程 | ⚪ | 新版自带 `growth_scheduler` |
| 42 | 插件申报定时动作 | ⚪ | 新版机制 `plugin_host/schedules` |
| 43 | 账号 store / 转移 / 身份 | ⚪ | 新版更完整（`account_transfer/identity` 等） |
| 44 | ZCode 领取 / billing | ⚪ | 新版自带 |
| 45 | CodeArts 福利 / 连登 | ⚪ | 新版自带 |
| 46 | server-only 启动脚本 | ✅ | `build-server.bat`（`--check` / `--test` / 默认 release）+ `start-plugin-server.ps1` |

### 界面与多语言（47–52）

| # | 功能 | 状态 | 说明 |
|---|---|---|---|
| 47 | 代理面板（订阅/节点/冷却/负载） | ✅ | 订阅管理重写（`proxies-sub-state.ts` 290 行 + `proxies-sub-view.tsx` 380 行 + 95 行样式）；冷却/负载按判定不做（见 #31–36） |
| 48 | 账号积分明细 / 余额弹层 | ⚪ | 新版自带（`accounts-credit-detail` 等）；本地扩展未比对 |
| 49 | 提供商标签 `provider-label.ts` | ❌ | 未移植 |
| 50 | 六语言词条 | ✅ | 本轮新增键（55 + 10 + 43 条旧键差异）已补齐六语 |
| 51 | 中文本地化文案 | ⚪ | 随新结构，中文即键 |
| 52 | ui-islands 构建与测试 | ✅ | 构建通过；`credit-measurement` + `settings-prompt-models` 两组 7 个用例全过（`node --test tests/*.test.mjs`） |

### 数据库与密钥（53–62）

| # | 项目 | 状态 | 说明 |
|---|---|---|---|
| 53 | `credit` 列冲突 | ✅ | v23 幂等 + v24 迁移；实测 `23→24` 列数不变 |
| 54 | `account_limiter_events` | ✅ | B0 补建并回填 19,154 条 |
| 55 | v18 六列 + 两索引 | ✅ | B0 补齐 |
| 56 | `assistant_images` | ✅ | B0 补建 |
| 57 | `request_raw.truncated` | ✅ | B0 补齐 + 回填 |
| 58 | `assistant_sessions.title_generated` | ✅ | B0 补齐 |
| 59 | 数据迁移结论 | ✅ | 桥接脚本 `03-bridge.py` + 副本演练 |
| 60 | `account.key` | ✅ | 随 B0 复制进测试目录，74 账号名称全部正确解密 |
| 61 | `update-token.key` | ❓ | 旧数据目录**只有** `account.key`；该密钥用途未确认（可能未启用） |
| 62 | `AGENT2API_ACCOUNT_KEY` | ❓ | 未验证新版是否仍读该环境变量 |

---

## 状态 C｜清单外新增能力

| 能力 | 位置 | 状态 |
|---|---|---|
| **出口就绪检查**（需求外新增） | `core/egress_ready.rs`（~200 行）+ `/v1` 闸门 + `RouteTarget.proxy_blocked` | ✅ 杀内核 → 2 秒自动拉起；准备期 `/v1` 503 而面板 200；运行期 502 拒绝而非直连 |
| **MiniMax 维护链路**（#5 补齐部分） | 插件 `providers/minimax/`：`oauth.rs` / `checkin.rs` / `usage.rs` / `methods.rs`（矩阵认证） | ✅ 签到 / 积分 / 远程目录 / 续期四项均实网验证 |
| **两跳断点修复**（#28 实测发现） | `account_store/snapshot.rs` / `store_view.rs` / `plugin_host/rpc.rs` | ✅ 三处形状/白名单缺口修复后真实转发穿透两跳 |
| **出口调度四件套**（#31–36） | `proxy_rotate/{health,admission,probe}.rs` + `proxy_settings` 3 键 + `proxies/rotate/*` 两个端点 | ✅ 冷却（含重启恢复）/ 探测 / 准入排队 / failureMode / Retry-After 全部实测 |

### 出口调度：为什么按语义重写而不是搬运

旧实现有 4755 行，深度耦合三处**新版已不存在**的基础设施：

| 旧依赖 | 新版现状 |
|---|---|
| `Attempts` / `ATTEMPTS` task-local（自带一次请求内的候选迭代与预算） | 无 —— 新版把候选迭代放在 `provider_loop` 的 `rotate_left` 循环里 |
| `sub_kernel::runtime::TemporaryLease` / 临时节点池 | 无 —— 新版只有 lazy listeners，没有「按需开临时节点」这条通道 |
| `warm`（预热 worker 提前持有 lease） | 依赖上一条，因此也不存在 |

搬运意味着连带搬三套基础设施，而那些在新版架构里**没有对应位置**。所以按语义重写：

| 语义 | 旧实现 | 新实现 | 位置 |
|---|---|---|---|
| 哪条出口现在别用 | `proxy_rotate.rs` 健康表 + `persistence.rs` | `health.rs`（同一份语义：指数退避 + 半开探测 + 只落哈希 + 代次防回写） | 822 行 |
| 冷却到期先确认 | `probe.rs`（临时节点预检） | `probe.rs`（通用出口探测：`HEAD origin/`，不带凭据） | 332 行 |
| 同时打几个请求 | `balance.rs`（按 origin + 出口 IP 的准入与排队） | `admission.rs`（同语义：按 `(origin, 出口)` 计数 + 有界队列 + 超时） | 466 行 |
| 什么时候换出口 | `Attempts::all_errors()` + `retry_status()` | `should_rotate_on` / `is_egress_failure`（判据集中在一处） | `proxy_rotate.rs` |
| 预热（提前建连） | `warm.rs` | **判定不做**：新版没有临时节点通道，预热无从谈起 |

**预热（`warm.rs` 829 行）不做**：它存在的前提是「临时节点需要现开」——预热的价值是在请求到来前把 lease 与探测做掉。新版没有临时节点通道（订阅节点是 lazy listener，内核常驻），这条通路不存在，预热也就没有可预热的对象。

### 28 的三处断点（实测才发现的）

「按账号测试」走的是**凭证**路径（`get_credentials_by_id` → `resolve_front_proxy(session)`），
而「真实转发」走的是**选路快照**路径（`routing_snapshot` → `RouteTarget.account`）。
两条路各自从不同投影取 `frontProxy`，于是出现「测试绿、转发只一跳」：

| # | 断点 | 现象 | 修法 |
|---|---|---|---|
| 1 | `snapshot::route_account` 的白名单**不含 `frontProxy`** | 选路快照里根本没有这个键，`resolve_front_proxy` 永远看到「没配」 | 白名单补齐：整份带过来（建链要用端口与凭据） |
| 2 | `plugin_accounts::to_plugin_public_account` 只写 `proxy` 不写 `frontProxy` | 插件家账号在公开投影里丢前置跳 | 补写，与内置家同形状 |
| 3 | `rpc::chained_proxy` 用 `ResolvedProxy::from_json(proxy)` 直解**展示形态** | 展示形态是 `{config:{host,port}}`，扁平解析必失败 → 静默回落单跳 | 与 `resolve_front_proxy` 同一条纪律：`config` 是对象就用它 |

另有第 4 处（非代码）：`frontProxy` 原先只写在**自定义账号**分支里，
从旧库桥接来的插件账号（没有 `pluginSecret`）走内置兜底形状 → 又被丢掉。
修法是把注入提到 `public_account` 的公共段，**所有形状**都从这里补齐。

---

## 状态 D｜未同步项汇总（按优先级）

> 截至本轮，原先的 10 个 🟡 与 **31–36 全部六项代理调度增强**都已转为 ✅。
> 下面只剩**判定为不做或未验证**的边角项。

### 中（功能缺口）

| 项 | 影响 |
|---|---|
| **49 提供商标签** | 界面徽章显示走新版兜底 |
| **9 WorkBuddy 出站身份按钮** | 未验证新版账号页是否仍需要它 |
| **11 地区收窄** | 同名模型跨地区选路可能不够精确 |
| **19 MiniMax 提示缓存断点** | 缓存命中率可能偏低（不影响正确性） |
| **27 保活/测试流量的积分处理** | 边角：保活流量可能不记 credit |

### 低（可用性与一致性）

| 项 | 影响 |
|---|---|
| **2/3/4 LobsterAI 指纹、Raccoon 指纹** | 判定不做（fork 成本） |
| **61/62 `update-token.key` / `AGENT2API_ACCOUNT_KEY`** | 未验证（旧数据目录没有前者的密钥文件） |
| **19 MiniMax 缓存断点** | 缓存命中率可能偏低（16 已补，19 未做） |
| **27 保活积分** | 保活流量的 credit 记账未验证 |

---

## 状态 E｜当前版本的关键事实

- **数据库**：`user_version = 24`，`integrity_check = ok`
- **内核**：自动拉起；冷启动实测 2–4 秒；92 个监听器在一次完整同步后稳定
- **转发安全**：账号「配了代理但不可用」时**拒绝**（502），不再回退直连
- **两跳**：真实转发实测穿透（`[front] CONNECT 127.0.0.1:18902` → `[back] CONNECT agent.minimax.cn:443`）
- **出口调度**：冷却重启不丢（实测 `♻️ 已恢复 1 条出口冷却` → `cooling:1 / failures:2`）；`failureMode` 三态往返正确（`smart` / `all_errors` / `bogus`→400）；准入三键越界返回 400、合法值落库
- **生产库**：`980ece1ba50e1b142acfed…` 全程未变（十批 + 本次改动）
- **测试**：`cargo test -p agent2api-server --lib` **399 passed / 0 failed**（其中出口调度 20 个）；`ui-islands` `node --test tests/*.test.mjs` **7 passed / 0 failed**；插件 `cargo test --bin agent2api-providers minimax` **10 passed / 0 failed**
- **构建**：`cargo build -p agent2api-server --release` 通过；`ui-islands` 的 `tsc + vite build` 通过（基线有 277 个既有 `TS7006`，与本轮无关）

### 本轮修掉的四个真实缺陷（都是实测才暴露的）

| # | 缺陷 | 后果 | 修法 |
|---|---|---|---|
| 1 | MiniMax 对 JWT 凭证走 Bearer（应走矩阵认证） | 从旧库桥接来的账号签到/积分恒 401，而推理端点正常 → 看起来像「账号坏了」 | `methods.rs` 新增 `control_request`，按 token 形态分派 |
| 2 | 控制面响应 `.json().unwrap_or(Null)` 吞掉 401 | 鉴权失败被伪装成「今天不在签到窗口」/「0 个积分包」 | `read_json`：非 2xx 一律 Err；`parse_status` 认不出形状也 Err |
| 3 | 选路快照白名单缺 `frontProxy`（+ 两处形状不匹配） | 两跳在真实转发路径上永远断在第一跳之前，而「按账号测试」绿 | 见「28 的三处断点」 |
| 4 | `usage_to_anthropic` 不认 `cache_creation_input_tokens` | Bedrock 路径上缓存写入被算进普通输入，客户端成本偏高 | 补写入项，并从普通输入里减掉 |

### 出口调度新增能力（#31–36）

| 文件 | 行数 | 职责 |
|---|---|---|
| `core/proxy_rotate/health.rs` | 822 | 健康表（指数退避 / 半开探测 / 代次防回写）+ SQLite 持久化（只落哈希）+ 重启恢复 |
| `core/proxy_rotate/admission.rs` | 466 | 按 `(origin, 出口)` 的准入：计数、有界队列、超时；`Permit` 用 Drop 释放 |
| `core/proxy_rotate/probe.rs` | 332 | `HEAD origin/` 探测：不带凭据、不跟跳转、任何状态码算通、singleflight |
| `core/proxy_rotate.rs` | 524 | 游标与候选（+ 冷却感知的 `next_for`）+ `failureMode` 判据 + `Retry-After` 解析 |
| `core/proxy_settings.rs` | 218 | 3 个新键（`rotateMaxConcurrentPerIp` / `rotateMaxQueueSize` / `rotateQueueTimeoutSeconds`）的读写与校验 |

**新端点**：`GET /api/proxies/rotate/cooldowns`（读摘要）、`POST /api/proxies/rotate/clear-cooldowns`（全清，只接受空 body）。

**接线点**（三处，都在 `provider_loop::attempt_custom`）：
1. 循环开头 → 冷却到期时先 `probe::check`，失败则 `note_probe_failure` + 换下一条；
2. 发送前 → `admit`（准入，失败返回 503 且**不**记冷却/账号限额）；
3. `Err` 分支 → `should_rotate_on` 决定换不换，`note_failure_unreserved` 记冷却。

---

## 一、提供商（Provider）

> ⚠️ **以下各节（一～十）是迁移开始前的初始分析，保留作为落点与依据的记录。**
> 每一条**当前做没做、做到什么程度**，一律以上面的「迁移状态总览」为准 ——
> 那里的「说明」列写明了缺口。两处若冲突，以总览为准（它是实测后的结论）。

| # | 本地功能 | 形态 | 新版落点 | 现状证据 |
|---|---|---|---|---|
| 1 | LobsterAI 全套（登录/续期/目录/签到/余额/SSE） | **P** | 插件已有 `providers/lobsterai/`（chat/checkin/credentials/oauth/usage/models/version） | 官方插件已实现主体；本地 `providers/lobsterai/*` 4492 行待比对差异 |
| 2 | LobsterAI 出站指纹（稳定账号标识 + 每请求 trace 分离） | **P** | `providers/lobsterai/` 新增 `fingerprint.rs` 等价模块 | 本地 `fingerprint.rs` 未提交，插件只有 `flight::fingerprint`；官方未实现该层 |
| 3 | LobsterAI 续期/签到增强 | **P** | 插件 `lobsterai` | 本地 `refresh.rs`、`checkin_tests.rs` 未提交 |
| 4 | Raccoon LLM 网关三层指纹头（20 项，含 Stainless / X-RACCOON / x-client-*） | **P** | 插件 `providers/raccoon/headers.rs` 扩充 | 插件现有 headers 仅 `X-Org-Code`；本地 `headers.rs` 未提交 |
| 5 | MiniMax Code（中国版，设备码 + PKCE、Anthropic→OpenAI 转换、签到、积分） | **P** | 插件需**新增** `minimax` provider | 插件 32 家**不含** minimax；宿主 `SUSPENDED_IDS` 含 `minimax-code`/`minimax-code-global`；本地 4465 行 |
| 6 | QwenWork 国内版（登录/签到/每日礼包/余额） | **P** | 插件 `providers/qoder/` 的 `Site::QwenworkCn` | 插件已有；本地 2682 行含 `daily_gift.rs`（余额对比式）需比对 |
| 7 | QwenWork 国际版 | **P** | 同上 `QwenworkIntl` | 新版 `SUSPENDED_IDS` 已停用 `qwenwork-intl`（登录回调受限），需确认是否恢复 |
| 8 | WorkBuddy 网页每日任务（`web_daily.rs`） | **P** | 插件 `providers/workbuddy/growth/` | 插件已有 growth 任务树；本地模块需比对 |
| 9 | WorkBuddy 出站身份按钮（`identity.rs`） | **H+U** | 宿主 `account_transfer/identity.rs` + 账号页 | 新版宿主无 `providers/workbuddy/identity.rs`；需确认账号页是否仍有该入口 |
| 10 | 会话粘性（session affinity，按对话固定账号命中 prompt cache） | **✔ 部分** | 宿主 `core/upstream/route_sticky.rs` | 新版已重写并增强（含 `sticky_list`/`sticky_clear`/TTL/profile）；旧实现内联在 `route_state.rs` |
| 11 | 地区收窄（Qoder/WorkBuddy 按 edition 收窄候选） | **H** | 宿主 `core/routing*` | 新版 `restrict_edition_candidates` 引用数 **0**，本地 **13**；属本地独有 |

---

## 二、协议与自定义提供商

| # | 本地功能 | 形态 | 新版落点 | 现状证据 |
|---|---|---|---|---|
| 12 | AWS Bedrock Converse + SigV4 + Event Stream 二进制分帧 | **X+H** | 宿主 `core/protocol/bedrock_outbound.rs` 重建 | 新版无此文件，`PROTOCOLS` 仅 `chat_completions/responses/anthropic`；本地 2593 行 |
| 13 | Vertex Gemini（请求/流翻译、thoughtSignature、usage） | **X+H** | 宿主 `core/protocol/vertex_outbound.rs` 重建 | 新版无此文件；本地 996 + signatures 171 行 |
| 14 | 自定义提供商协议扩展（bedrock/vertex 进 `PROTOCOLS`） | **H** | 宿主 `core/custom_providers.rs` | 新版 `PROTOCOLS` 3 项，本地 5 项 |
| 15 | 插件能否承载 Bedrock/Vertex | **受限** | 插件 `UpstreamKind` 仅 5 种（chat/anthropic/ndjson/gemini/chatglm_snapshot） | Bedrock Event Stream 与 Vertex 信封**不在**枚举内，插件无法自行声明新流协议 → 必须宿主加翻译机 |
| 16 | Anthropic 透传 `cache_creation_input_tokens` | **H** | 宿主 `core/protocol/anthropic*` | 需比对新版是否已覆盖 |
| 17 | 提示词按模型范围生效（`parse_model_scope`，`[]` vs 未给） | **H+U** | 宿主 `core/prompt.rs` + `settings-prompt-editor.tsx` | 新版 `parse_model_scope` 引用 **0**，本地 **9**；新版 `prompt.rs` 只有 `for_provider` |
| 18 | 提示词「适用模型」选择器 | **U** | `ui-islands/src/islands/settings-prompt-*.tsx` | 新版无 `settings-prompt-model-picker.tsx` / `settings-prompt-models.ts` |
| 19 | MiniMax 提示缓存断点 | **P** | 插件 `minimax` | 依附 #5 |
| 20 | 图片压缩（内联 base64 长边压缩） | **✔** | 宿主 `core/upstream/image_compress.rs` | 新版新增能力，旧版没有；直接使用 |

---

## 三、积分与用量统计

| # | 本地功能 | 形态 | 新版落点 | 现状证据 |
|---|---|---|---|---|
| 21 | `requests.credit` 落库（上游 `usage.credit`） | **H** | 宿主 `api/pipeline.rs` 补 `entry.credit = snapshot.credit` | **新版 schema v23 建了列，但 `api/pipeline.rs` 里 `credit` 出现次数为 0**；`snapshot.credit` 采集到了却从不写库 —— 属上游未接完的缺口 |
| 22 | `credit_source`（区分实报 / 估算） | **H+D** | 宿主 `db/schema.rs` 新增列 + `request_stats` 聚合 | 新版全文无 `credit_source`；本地 61 处引用 |
| 23 | 积分估算器（`credit_estimate.rs`，按 token 折算 + `estimated` 标记） | **H** | 宿主 `core/upstream/credit_estimate.rs` | 新版无此文件；本地 305 行。注意本地对 raccoon/loomy/lobsterai/minimax 仍 `scale=1.0` 占位，非实测标定 |
| 24 | 报表按账号积分聚合（`push_account_credit`、覆盖率、`credit_estimated`） | **H** | 宿主 `request_stats/report.rs` | 新版 `request_stats/report.rs` 中 `credit` 出现 **0** 次；本地已实现 |
| 25 | 前端积分列（`≈` 估算标记、部分覆盖、`credit-measurement.ts`） | **U** | `ui-islands/src/islands/credit-measurement.ts` + 报表页 | 新版无 `credit-measurement.ts`；`report-page.tsx` 中 `credit` 出现 0 次 |
| 26 | 请求日志「消耗」列 | **U** | 请求面板 | 依附 #21/#22 |
| 27 | 保活/测试流量的积分处理 | **H** | `core/billing/keepalive.rs` | 新版该处不写 credit |

---

## 四、代理与出口

| # | 本地功能 | 形态 | 新版落点 | 现状证据 |
|---|---|---|---|---|
| 28 | 前置代理两跳链（`frontProxy` + 本地 CONNECT 中转） | **X+H** | 宿主 `core/proxy_chain.rs` 重建 + `core/egress.rs` | 新版 `frontProxy` 引用 **0**（本地 161）；新版无 `proxy_chain.rs`。**插件侧也无法承载**：`WireProxy` 只有单跳字段 |
| 29 | 代理订阅 + 内置 mihomo 内核（`sub_kernel/`，下载/校验/进程/节点同步） | **X** | 新版**已整体下线** | `core/proxies.rs` 明确「旧订阅来源保留记录但解析为不可用」；`proxy_pool.rs` 有 `RETIRED_HINT`；路由 `/api/subscriptions/*` 全部返回 `retired_proxy_feature`；本地 3592 + 734 行 |
| 30 | 订阅导入（链接 / 本地文件 / 二维码 / 原地换文件） | **X** | 同上，需重建 | 本地 12 条 `/api/subscriptions/*` 路由，新版合并为 2 条退役路由 |
| 31 | 轮询失败模式 `failureMode`（smart / all_errors） | **H** | 宿主 `core/proxies.rs` + `proxy_rotate.rs` | 新版 `failureMode` 引用 **0**；本地已实现 |
| 32 | 出口冷却持久化（重启不丢，哈希键 + origin 隔离） | **H+D** | 宿主 `proxy_rotate/persistence.rs` 重建 | 本地 512 行 + 新增 SQLite 表；新版 `proxy_rotate.rs` 仅 236 行，无持久化 |
| 33 | 出口健康探测 / 预热（`probe.rs`、`warm.rs`） | **H** | 宿主 `proxy_rotate/` | 本地 290 + 829 行；新版无 |
| 34 | 出口准入与负载均衡（`balance.rs`，按 origin + 出口 IP 限并发/排队/超时） | **H** | 宿主 `proxy_rotate/balance.rs` + `proxy_settings` | 新版 `proxy_settings.rs` 仅 87 行（本地 300+，含 `rotateMaxConcurrentPerIp` 等 3 个键） |
| 35 | 请求级出口均衡（`request_balance.rs`） | **H** | 宿主 `core/upstream/request_balance.rs` | 本地 127 行；新版无 |
| 36 | 429 / `Retry-After` 出口冷却（`request_recovery.rs`） | **H** | 宿主 `core/upstream/request_recovery.rs` | 本地 321 行；新版无 |
| 37 | 维护链路严格出口（`provider_route.rs`：签到/余额/目录/续期/探测，配置了代理就必须用） | **H** | 宿主 `core/provider_route.rs` | 本地 277 行、73 处调用；新版无此文件 |
| 38 | 节点可见性 / 候选数 / 冷却 / 负载面板 | **U** | `ui-islands/src/islands/node-visibility*.ts(x)` + 测试 | 新版无 `node-visibility*`、`proxy-cooldowns/load-balance/candidate-count` 测试 |
| 39 | 新版网络回退（备用入口 + DNS 容错） | **✔** | 宿主 `core/upstream/network_fallback.rs` | 新版新增，旧版没有；与 #36 不冲突，可并存 |

---

## 五、账号、调度与业务

| # | 本地功能 | 形态 | 新版落点 | 现状证据 |
|---|---|---|---|---|
| 40 | 签到末尾补签 / 兑换档位 / 抽奖 | **✔** | 宿主 `core/billing/checkin.rs`、`ui-islands/checkin-*` | 新版 `checkin-makeup-*`、`checkin-center-tab` 齐备 |
| 41 | 猫猫旅行定时排程（北京整点派出 + 领奖） | **✔** | 宿主 `core/growth_scheduler.rs` | 新版已有 |
| 42 | 插件申报定时动作（`schedules.list`，时刻/开关归宿主） | **✔** | `core/plugin_host/schedules.rs` | 新版机制；provider 级定时动作迁插件后自动获得 |
| 43 | 账号 store / 优先级 / 转移 / 身份 | **✔ 部分** | `core/account_store/*`、`core/account_transfer/` | 新版更完整（`account_transfer/identity.rs` 等）；本地新增的 `*_accounts.rs`（lobsterai/minimax/qwenwork）随 provider 迁插件 |
| 44 | ZCode 领取 / billing 调度 / 活动任务 | **✔** | `core/billing/*`、`api/zcode_claim.rs` | 需比对行为差异 |
| 45 | CodeArts 福利 / 连登管家 | **✔** | `api/codearts_welfare.rs`、`checkin_records.rs` | 新版已有对应文件 |
| 46 | server-only 启动/构建约束（`run-server.ps1`、`build-installer.bat`、`.cargo/config.toml`） | **X** | 项目根重建 | 新版无 `run-server.*`、无 `.cargo/config.toml`；已写入 `start-plugin-server.ps1` 替代 run-server |

---

## 六、界面与多语言

| # | 本地功能 | 形态 | 新版落点 | 现状证据 |
|---|---|---|---|---|
| 47 | 代理面板（订阅管理、节点开关、冷却、负载、候选） | **U** | `ui-islands/proxies-*.ts(x)` | 新版代理页仍在但**无订阅/内核/冷却/负载**相关组件 |
| 48 | 账号积分明细 / 余额弹层 | **U 部分** | `accounts-credit-detail.tsx`、`accounts-dialog-usage.tsx` | 新版有类似组件（`credit_detail` 能力位），需比对本地扩展 |
| 49 | 提供商标签（`provider-label.ts`） | **U** | `ui-islands/src/islands/provider-label.ts` | 新版**无**该文件 |
| 50 | 六语言词条（本地新增约 174 条 en 键） | **U** | `i18n/i18n-src/<lang>/*.json` + 运行时 `ui/i18n/` | 新版新增了 `account-settings.json`、`routing.json`，同时目录重构（`update.json` 消失）。实测 en 缺失：accounts 69、common 23、proxies 80、settings 20 |
| 51 | 中文本地化文案（中文为源语言，直接写 key） | **U** | 同上 | 迁移时需按新键名重新落位，不能整文件覆盖 |
| 52 | `ui-islands` 构建与测试（本地 6 个未跟踪测试） | **U** | `ui-islands/tests/*.mjs` | 新版测试集不同，需按新结构改写 |

---

## 七、数据库与密钥

| # | 项目 | 形态 | 处理方式 | 证据 |
|---|---|---|---|---|
| 53 | `requests.credit` 列 | **D 冲突** | 新版 v23 `ALTER TABLE requests ADD COLUMN credit REAL` 会在旧库上**报错**（`duplicate column name: credit`）。已在库副本实测复现。必须让迁移对已有列幂等（`has_column` 判断） | 本地库 `user_version=17` 且已有 `credit`、`credit_source`、`route` |
| 54 | `account_limiter_events` 表（官方 v17 新建） | **D 缺失** | 旧库 `user_version=17`，新版认为 v17 已完成 → **不会**建这张表，运行期 `core/limiter/counter.rs` 直接查询会失败 | 实测旧库无此表；新库有 |
| 55 | v18 新增列（`api_key_id/name/source`、`client_stats`、`api_key_stats`、`source_stats_version`） | **D 缺失** | 同上被跳过 | 实测旧库全部缺失 |
| 56 | v19 `assistant_images` 表 | **D 缺失** | 同上被跳过 | 实测旧库无此表 |
| 57 | v20 `request_raw.truncated` | **D 缺失** | 同上 | 实测旧库缺失 |
| 58 | v21 `assistant_sessions.title_generated` | **D 缺失** | 同上 | 实测旧库缺失 |
| 59 | 数据迁移结论 | **D** | **不能直接让新版打开旧库**。需先做「版本号重映射」：把旧库 `user_version` 降到官方 17 对应的真实结构点，或写一次性桥接脚本补齐 v17–v22 的 DDL，再让程序跑 v23 | 迁移器按 `user_version` 单调推进，同版本直接返回，不校验真实列 |
| 60 | `account.key`（账号域加密密钥） | **D** | 整库备份**不含**此文件，跨机迁移必须单独携带 | `core/db/backup.rs` 有明确警告 |
| 61 | `update-token.key` | **D** | 同上（本地数据目录当前只有 `account.key`，需确认是否用环境变量注入） | `core/crypto.rs` |
| 62 | `AGENT2API_ACCOUNT_KEY` 环境变量注入 | **D** | 沿用同一变量名 | `core/crypto.rs` |

---

## 八、新版已删除、必须接受的减法

| 能力 | 影响 |
|---|---|
| 代理订阅 + 内置代理内核 | 若 #29/#30 不重建，订阅节点、二维码导入、内核下载全部不可用，只剩 HTTP/SOCKS5 手动地址与 Clash Verge 同步 |
| 模型调用默认要求有效网关 Key | 首次使用需先建 Key，否则 `/v1/*` 拒绝 |
| 备份改为密码加密 `.a2ab` | 旧 `.json`/SQL 备份导入路径需确认 |
| Trae 模型名去掉 `-solo` 后缀 | 存量配置有新版本内置迁移，需验收 |

---

## 九、迁移批次建议（按依赖排序）

| 批次 | 内容 | 形态 | 前置 |
|---|---|---|---|
| B0 | 数据库桥接脚本 + 副本演练（#53–#58） | D | 无 |
| B1 | 代理核心：两跳、严格出口、冷却持久化、均衡、失败模式（#28、#31–#37） | H+D | B0 |
| B2 | 积分链路：落库 → 来源 → 估算 → 聚合 → 前端（#21–#27） | H+D+U | B0 |
| B3 | 协议：Bedrock + Vertex + 自定义协议 + 提示词范围（#12–#18） | H+U | B0 |
| B4 | 订阅与内核（#29、#30） | X | B1（出口语义需先稳定） |
| B5 | Provider 插件：MiniMax 新建、LobsterAI/Raccoon 增强（#1–#5） | P | 官方插件基线 |
| B6 | 界面与多语言（#38、#47–#52） | U | B1–B3 的接口定型 |
| B7 | 本地运维脚本与 server-only 约束（#46） | X | 无 |

---

## 十、尚未验证

- 新版宿主其余 provider 的内部实现与本地差异（仅比对到文件级与能力位）。
- 官方插件 32 家的行为与本地对应家是否等价（未做逐家回归）。
- 新版 `account_store` / `billing` 的字段级差异（只比对了表结构与能力位）。
- 本地 189 个未暂存修改中没有逐个文件读完全部内容，按目录与功能聚类统计。
- 数据库桥接脚本尚未编写，上述冲突为静态分析 + 库副本单点实测（`ALTER` 报错、缺表缺列）。
