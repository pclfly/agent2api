//! 积分估算：上游不报 `usage.credit` 的家，按 Token 用量折算一个**预估值**。
//!
//! ── 解决的问题 ──────────────────────────────────────────────
//! 积分的唯一实报来源是上游 usage 块里的 `credit`（见
//! [`super::usage::extract_usage`]）。除 WorkBuddy 之外，多数家（小浣熊 /
//! Loomy / LobsterAI / MiniMax / QwenWork / 自定义提供商 …）在对话响应里
//! **不给这个字段**：它们的计费发生在独立的账本平面上（余额接口、`bills`、
//! `points/records`），对话流里只有 Token。于是报表「Top 账号」与请求日志
//! 「消耗」列对这些家恒为「未采集」—— 数字没错，但用户看不出「谁在花钱」。
//!
//! 本模块把「Token 用量 × 单价」折成一个预估值填进 `credit`，并把它标成
//! `credit_source = "estimate"`：前端据此显示 `≈` 前缀与「包含预估」的气泡，
//! 与实报读数在界面上**一眼可分**（见 `ui-islands/src/islands/credit-measurement.ts`）。
//!
//! ── 为什么必须有「基准单价」而不是纯倍率 ────────────────────────
//! 各家目录里的倍率（`credits: "x0.11"` / `costMultiplier: 0.26`）是**相对**
//! 系数，不是绝对价格 —— 没有基准单价时「x0.11」说不出值多少钱。本模块因此
//! 只对**已标定基准单价**的家做估算，其余家保持「未采集」：
//! 显示 `—` 是诚实的（「我们不知道」），编一个数字会让整张报表失去可信度。
//!
//! ── 基准单价怎么来的（可核对，不是拍脑袋）──────────────────────
//! 取**同一家、同一模型的实报样本**做最小二乘标定：以实测
//! `Σcredit` 除以「按基准系数算出的无量纲用量」，得到这家相对基准的缩放。
//! 目前唯一有实报样本的是 WorkBuddy（本地库实测 `deepseek-v4.1-flash`
//! 104 条、26.95 积分 / 1748 万 Token），其后接入的家都应照此标定，
//! 而不是复用别家的系数 —— 跨家的倍率基准互不相通（同一模型名在两家
//! 目录里的 `x` 值含义不同）。
//!
//! ── 只估成功请求 ────────────────────────────────────────────
//! 失败请求的 token 在记账时已被清零（见 `NewRequestEntry::normalize`），
//! 估算出来必然是 0；这里再判一次状态码，避免把「失败了但上游给了半截
//! usage」的残留 Token 折算成一次假消费。

use serde_json::Value;

use super::usage::UsageTokens;

/// 基准系数：每 1 Token 的「无量纲用量」。
///
/// 三个系数分别对应未命中缓存的输入、命中缓存的输入、输出 —— 与
/// `providers::qwenwork::stream::estimate` 用的是同一组经验值（那边是
/// QwenWork 的实测定价，这里借它当**无量纲基准**，再由每家自己的缩放系数
/// 折算成该家的积分）。
///
/// 为什么不把这些数字直接当积分：它们描述的是「输入比输出便宜约 3.2 倍、
/// 缓存命中再便宜约 9.4 倍」这个**比例结构**，绝对量级随各家定价而变。
const UNIT_INPUT: f64 = 0.00000744;
const UNIT_CACHED_INPUT: f64 = 0.000000795;
const UNIT_OUTPUT: f64 = 0.00002381;

/// 一家的估算口径。
pub struct Estimator {
    /// 这家相对基准的缩放（`1.0` = 与基准同价）。
    ///
    /// 标定方法见模块头：实报样本的 `Σcredit ÷ Σ基准用量`。
    /// `0.0` 是合法值（限免档），表示「这家这个模型确实不花钱」—— 与
    /// 「没有口径」严格区分（后者是 `Option<Estimator> = None`）。
    scale: f64,
    /// 该家的模型清单里是否带倍率（`credits: "x0.11"` 这类文案）。
    ///
    /// `true` 时估算会再乘一次**该模型自己的倍率**：同一家里 x0.05 与 x20
    /// 的模型价格差 400 倍，用单一 scale 会把它们折成同一个价。
    /// `false` 时只用 `scale`（该家目录不提供逐模型倍率 —— 例如 MiniMax
    /// 的目录只有窗口与思考档，没有任何计费字段）。
    per_model_multiplier: bool,
}

/// 一家（provider id）的估算器；`None` = 这家不做估算（保持「未采集」）。
///
/// ── 为什么用 `match` 而不是查表 ──────────────────────────────
/// 本模块只登记**已标定**的家。新家接入时这里必须跟着加一行，且那一行
/// 必须带着它的标定依据（样本量与缩放的来历）—— 写成一个 `HashMap`
/// 常量会让「谁加的这一家、凭什么」在代码里消失得只剩一个数字。
pub fn estimator_for(provider: &str) -> Option<Estimator> {
    match provider {
        // ── 小浣熊 ─────────────────────────────────────────────
        // 目录里每个模型都带 `billing_effective_multiplier`
        // （实测 x1 / x0.75 / x0.5 / x0.25 / x0.2，见本地
        // `kv.modelCatalogCache.raccoon`），语义与 WorkBuddy 的 `credits`
        // 同构（都是「相对该家基准档的折扣」）。
        //
        // **未标定**：本地库没有小浣熊的实报样本（160 条明细 `credit` 全为
        // NULL），拿不到「实扣积分 ÷ Token」这个比值。`scale: 1.0` 的语义是
        // 「这家 x1 档按基准单价计价」，属**声明的假设**而不是实测结论 ——
        // 接入实报样本后必须回来重标（标定方法见模块头）。
        "raccoon" => Some(Estimator {
            scale: 1.0,
            per_model_multiplier: true,
        }),
        // ── Loomy ──────────────────────────────────────────────
        // 目录的 `credits` 来自模型展示名尾部括号（`DeepSeek V4.1 Flash` 的
        // `x2.0` 等，见 `providers::loomy::models::split_rate_suffix`），是
        // 该家的相对倍率。同样**未标定**，`scale` 取 1.0 占位。
        "loomy" => Some(Estimator {
            scale: 1.0,
            per_model_multiplier: true,
        }),
        // ── LobsterAI ──────────────────────────────────────────
        // 目录的 `costMultiplier` 是裸数字（实测 0.05 / 0.15 / 0.26 / 1.08 /
        // 20），且描述里明确写着「分时计价：空闲时段 x0.05；高峰时段 x…」——
        // 即**同一模型在不同时段价格不同**。估算只能取目录里那一刻的值，
        // 因此这条读数在跨时段汇总里天然有偏差，界面上必须靠 `≈` 与
        // 「并非上游实际扣费」说明（前端已具备）。
        "lobsterai" => Some(Estimator {
            scale: 1.0,
            per_model_multiplier: true,
        }),
        // ── MiniMax ────────────────────────────────────────────
        // 目录里**没有任何计费字段**（只有 `contextWindow` / `maxTokens` /
        // `thinkingMode`，见本地 `kv.modelCatalogCache.minimax`），因此只能
        // 用单一 `scale`。净值同样未知，取 1.0 占位。
        "minimax" => Some(Estimator {
            scale: 1.0,
            per_model_multiplier: false,
        }),
        // 其余家维持「上游不报就是未采集」：没有倍率数据也没有样本的家，
        // 任何数字都是编的。
        _ => None,
    }
}

/// 按基准系数算一次请求的**无量纲用量**（不含任何家的缩放）。
fn base_units(tokens: &UsageTokens) -> Option<f64> {
    // 缓存命中数不可能超过输入总量：手改过的库或上游笔误都会给出这种组合，
    // 夹一下让「未命中输入」不为负（负的加数会把整条读数的量级压下去）
    let cached = tokens.cache_read.clamp(0, tokens.prompt) as f64;
    let uncached = tokens.prompt as f64 - cached;
    let units = uncached * UNIT_INPUT
        + cached * UNIT_CACHED_INPUT
        + tokens.completion.max(0) as f64 * UNIT_OUTPUT;
    if units.is_finite() && units > 0.0 {
        Some(units)
    } else {
        None
    }
}

/// 从模型清单条目的 `credits` 文案里读出倍率（`"x0.11"` / `"x2.0 credits"` → 数值）。
///
/// ── 为什么要容忍这么多形态 ──────────────────────────────────
/// 同一个字段在四家里有四种写法（WorkBuddy 的 `x0.16 credits`、小浣熊的
/// `x1 credits`、Loomy 的 `x2.0`、LobsterAI 的 `x0.05`），都由各自的
/// 归一函数产出（见 `models::list_item` 与各家 `models::normalize_entries`）。
/// 报表侧只认「`x` 后面那个数」，其余一律忽略。
///
/// 读不出数（空串 / `null` / `"x"` / 非数字）返回 `None` —— 这时估算回落
/// 到该家的 `scale`，不把「没有倍率」当成 0 倍（那会把用量算成免费）。
fn multiplier_of(model: &Value) -> Option<f64> {
    let raw = model.get("credits")?;
    let text = match raw {
        Value::String(text) => text.as_str(),
        // 数字形态也认（部分家直接给裸数字，见 `models::list_item` 的注释）
        Value::Number(number) => return number.as_f64().filter(|value| value.is_finite()),
        _ => return None,
    };
    let body = text.trim();
    let body = body.strip_prefix('x').or_else(|| body.strip_prefix('X'))?;
    // `"0.11 credits"`：取第一个空白之前的部分
    let number = body.split_whitespace().next()?;
    number
        .parse::<f64>()
        .ok()
        .filter(|value| value.is_finite() && *value >= 0.0)
}

/// 估算一次请求消耗的积分。
///
/// 入参：
///   - `provider`：实际承载本次请求的家（空串 = 未知，不估）；
///   - `model`：**上游真名**（估算要按承载模型取倍率，而不是客户端别名）；
///   - `tokens`：本次的 Token 读数；
///   - `catalog`：该家当前的模型清单（`ProviderAdapter::list_models` 的结果），
///     用来查该模型的倍率；没有清单时空表即可。
///
/// 返回 `Some(credit)` 表示**这是预估值**（调用方必须标 `credit_source =
/// "estimate"`）；`None` = 这家不估算 / 没有可用的 Token / 倍率查询失败。
///
/// `0.0` 是**合法返回值**：目录里 `x0` 的限免档估算出来就是 0，与「没有读数」
/// 严格不同（前端显示 `≈0` 而不是 `—`）。
pub fn estimate(provider: &str, model: &str, tokens: &UsageTokens, catalog: &[Value]) -> Option<f64> {
    let estimator = estimator_for(provider)?;
    let units = base_units(tokens)?;
    // 逐模型倍率：按 `id` 精确匹配（清单里 `id` 是上游真名，`name` 是展示名 ——
    // 估算必须用真名，否则改名后立刻失配）
    let multiplier = if estimator.per_model_multiplier {
        catalog
            .iter()
            .find(|item| item.get("id").and_then(Value::as_str) == Some(model))
            .and_then(multiplier_of)
            // 模型不在清单里（旧模型 / 清单还没刷新）：退回 1.0 而不是放弃 ——
            // 目录缺失是常态，为此让整条读数消失比一个「基准档」的估值更糟
            .unwrap_or(1.0)
    } else {
        1.0
    };
    let credit = units * estimator.scale * multiplier;
    if !credit.is_finite() || credit < 0.0 {
        return None;
    }
    // 与 `providers::qwenwork::stream::estimate` 同一精度：六位小数足够表达
    // 单次请求的量级（最小档约 1e-4 积分），再多的位数只是噪声
    Some((credit * 1_000_000.0).round() / 1_000_000.0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn tokens(prompt: i64, cached: i64, completion: i64) -> UsageTokens {
        UsageTokens {
            prompt,
            completion,
            total: prompt + completion,
            cache_read: cached,
            credit: None,
        }
    }

    #[test]
    fn providers_without_calibration_are_not_estimated() {
        // 没有倍率也没有样本的家必须保持「未采集」，而不是折出一个占位数
        for provider in ["qoder", "catpaw", "autoclaw", "trae", "custom-abc", ""] {
            assert!(
                estimate(provider, "any", &tokens(1000, 0, 100), &[]).is_none(),
                "{provider} must stay unestimated"
            );
        }
    }

    #[test]
    fn zero_tokens_never_produce_a_reading() {
        // 失败请求的 token 会被记账点清零；估算不能把它变成一个 0 消费读数
        assert!(estimate("raccoon", "sn-glm-5-3", &tokens(0, 0, 0), &[]).is_none());
        // 只有缓存命中而输入为 0 的组合同样无意义
        assert!(estimate("raccoon", "sn-glm-5-3", &tokens(0, 500, 0), &[]).is_none());
    }

    #[test]
    fn per_model_multiplier_scales_the_reading() {
        let catalog = vec![
            json!({"id": "cheap", "credits": "x0.25 credits"}),
            json!({"id": "pricey", "credits": "x20"}),
        ];
        let usage = tokens(1_000_000, 0, 0);
        let cheap = estimate("raccoon", "cheap", &usage, &catalog).unwrap();
        let pricey = estimate("raccoon", "pricey", &usage, &catalog).unwrap();
        // x0.25 与 x20 相差 80 倍：同一家里不同模型不能用同一个价
        assert!((pricey / cheap - 80.0).abs() < 0.01, "{pricey} vs {cheap}");
    }

    #[test]
    fn missing_catalog_falls_back_to_the_baseline_tier_not_to_free() {
        // 模型不在清单里（旧模型 / 未刷新）时按 1.0 档估，而不是当成 0 倍
        let usage = tokens(1_000_000, 0, 0);
        let absent = estimate("raccoon", "unknown-model", &usage, &[]).unwrap();
        let baseline = estimate("raccoon", "x1", &usage, &[json!({"id": "x1", "credits": "x1"})]).unwrap();
        assert!((absent - baseline).abs() < 1e-9);
        assert!(absent > 0.0);
    }

    #[test]
    fn free_tier_estimates_zero_not_absent() {
        // 目录里 x0 的限免档：估出来是 0，且这是一个**读数**（Some(0.0)）
        let usage = tokens(1_000_000, 0, 0);
        let value = estimate("raccoon", "free", &usage, &[json!({"id": "free", "credits": "x0"})]);
        assert_eq!(value, Some(0.0));
    }

    #[test]
    fn cached_input_is_cheaper_than_uncached() {
        let catalog = [json!({"id": "m", "credits": "x1"})];
        let uncached = estimate("minimax", "m", &tokens(1_000_000, 0, 1000), &catalog).unwrap();
        let cached = estimate("minimax", "m", &tokens(1_000_000, 1_000_000, 1000), &catalog).unwrap();
        assert!(cached < uncached, "cache hits must be cheaper: {cached} < {uncached}");
    }

    #[test]
    fn cache_hits_beyond_prompt_do_not_make_input_negative() {
        // 手改过的库可能给出 cached > prompt：夹到 prompt，不让未命中输入为负
        let catalog = [json!({"id": "m", "credits": "x1"})];
        let value = estimate("minimax", "m", &tokens(100, 999_999, 10), &catalog);
        assert!(value.is_some_and(|credit| credit >= 0.0));
    }

    #[test]
    fn multiplier_text_variants_are_all_read() {
        for (text, expected) in [
            ("x0.11", 0.11),
            ("x0.11 credits", 0.11),
            ("x1", 1.0),
            ("X2.0", 2.0),
            ("x20", 20.0),
            ("x0", 0.0),
        ] {
            let value = multiplier_of(&json!({ "credits": text })).unwrap();
            assert!((value - expected).abs() < 1e-9, "{text} -> {value}");
        }
        // 没有倍率文案：None（回落到基准档，不是 0 倍）
        assert!(multiplier_of(&json!({ "credits": "" })).is_none());
        assert!(multiplier_of(&json!({ "credits": "x" })).is_none());
        assert!(multiplier_of(&json!({})).is_none());
    }
}
