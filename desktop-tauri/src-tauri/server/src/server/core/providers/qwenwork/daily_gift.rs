//! QwenWork 每日登录积分的**余额对比**（上游没有可调用的领取接口）。
//!
//! ── 为什么没有「签到请求」─────────────────────────────────────
//! 客户端 bundle（国际版 1.0.4-26090501）里 `/sash` 家族只有 6 条路径
//! （invitationCode / invitationRewards / daily-check-in status+claim /
//! pro-upgrade eligibility+claim），**没有任何「每日登录赠送」相关端点**，
//! 也搜不到对应文案；参考实现（`qw2api`）同样只做余额追踪。
//!
//! 线上实测（2026-10-15/16，国际版账号，只读）：
//!   · `/sash/api/v1/me/daily-check-in/status` 等 `/sash/*` 全部 **404**；
//!   · `/api/v1/adapter/user/*` 下 credit / credits / reward / rewards / daily /
//!     task / tasks / checkin / signin / grant / benefit 等 60 余个候选全部 404；
//!   · 能通的只有 account-context、quota/usage、user/plan、user/status、
//!     userinfo、prompts/recommended、auth/identities、ide-text/latest、
//!     qcs/config/resolve；
//!   · 按客户端启动序列把上述接口各打一遍，`quota.remaining` **不变**（700）。
//!
//! 结论：那笔积分由**服务端在当天首次真实使用（对话）时或按自然日定时入账**，
//! 客户端与网关都没有「点一下领」的入口。所以这一家在本链上能做的只有两件：
//!   ① 查一次余额（`account-context` 的 `quota.remaining`，只读）；
//!   ② 与**上次成功读到的跨日读数**对比 —— 变多就是当天已到账，没变就是尚未发放。
//!
//! ── 与别家签到的差别（如实表达，不假装领到了）─────────────────
//! 别的家「签到成功」意味着本次请求让上游发了一笔积分；这里**没有任何请求
//! 让它发积分**，所以：
//!   · 余额变多 → `success:true` + 「每日登录积分已到账（+N）」，落今日台账；
//!   · 余额没变 → `success:false` + 「今日尚未发放」，**不落台账**（今天还
//!     可能到账），走 `claim_pending` 的中性路径（定时链稍后重试）；
//!   · 余额读不到 → 报错（凭证 / 网络问题），与别家的失败同档。
//! 绝不把「余额没变」写成「今日已签到」——那会让用户以为今天已经领过了。
//!
//! ── 跨日读数存在哪 ────────────────────────────────────────────
//! 不新开存储：`account_usage_records`（余额快照表）里那一行就是「上次读到的
//! 余额 + 读数时刻」。本模块读它、比对日期，然后把本次读数写回去
//! （`usage_records::write_result` 的既有入口，与余额查询链同一份事实）。
//! 因此**每天第一次签到**就能给出结论：库里存着昨天的读数，今天一读一比即可。

use super::{auth, balance, credentials::text, refresh};
use crate::server::core::account_store::AccountStore;
use crate::server::core::beijing;
use crate::server::core::usage_records;
use crate::server::errors::GatewayError;
use serde_json::{json, Value};

/// 今日余额相对上次跨日读数的差额（正数 = 今天到账了多少）。
///
/// `None` = 判不出来（库里没有读数、上次读数就是今天、或上次读数不合法）——
/// 调用方据此给中性结论而不是猜一个数字。
fn credited_since_last_day(account_id: &str, remaining: f64, now_ms: i64) -> Option<f64> {
    let record = usage_records::load(account_id);
    let previous = record.remaining?;
    if !previous.is_finite() || !remaining.is_finite() {
        return None;
    }
    // 上次读数是不是**今天之前**的：同一天内的读数不参与对比（一天里可能读
    // 好几次余额，拿半小时前的读数比会把「这半小时没消耗」说成「到账 0」）
    let last_at = record.last_success_at;
    if last_at <= 0 {
        return None;
    }
    if beijing::date_key_of_ms(last_at) == beijing::date_key_of_ms(now_ms) {
        return None;
    }
    Some(remaining - previous)
}

/// 一次「每日登录积分」核对的结果。
///
/// `credited` 是**当天到账的净增量**（余额差值），不是上游报出的发放额 ——
/// 上游不报发放额，而余额同时受消耗影响。所以文案里只说「余额比上次跨日读数
/// 多/少了 N」，不说「赠送了 N」。这一点在 UI 与文档里都要一致。
pub struct DailyGiftOutcome {
    pub remaining: f64,
    /// 判不出增量时是 None（第一次读、或今天已经读过一次）
    pub credited: Option<f64>,
}

impl DailyGiftOutcome {
    /// 翻成签到结果行的 claim 形状（`{success, msg, rewardPoints?}`）。
    ///
    /// 三档与模块头的口径一一对应：
    ///   · 增量 > 0 → success（今天确实到账了，落今日台账）；
    ///   · 增量 ≤ 0 或判不出 → `success:false` + 中性文案，不落台账
    ///     （`claim_pending` 认「尚未发放」那句，定时链稍后重试）；
    ///   · 余额读不到 → 由调用方报错，不走这里。
    pub fn to_claim(&self) -> Value {
        match self.credited {
            Some(delta) if delta > 0.0 => json!({
                "success": true,
                "msg": format!("每日登录积分已到账（余额 {:.3}，比上次读数多 {:.3}）", self.remaining, delta),
                "rewardPoints": delta,
                "unit": "积分",
            }),
            Some(delta) => json!({
                "success": false,
                // 中性未领取：今天还可能在首次对话时到账，定时链稍后重试
                "msg": format!("今日尚未发放（余额 {:.3}，比上次读数少 {:.3}）", self.remaining, -delta),
                "unit": "积分",
            }),
            None => json!({
                "success": false,
                "msg": format!("今日尚未发放（余额 {:.3}；下次签到可对比出今天的增量）", self.remaining),
                "unit": "积分",
            }),
        }
    }
}

/// 核对一次「每日登录积分」：读余额 → 与上次跨日读数对比 → 记下本次读数。
///
/// 只有一次只读请求（`account-context`），**不发任何会消耗积分的调用**。
pub async fn check_daily_gift(
    store: &AccountStore,
    id: &str,
) -> Result<DailyGiftOutcome, GatewayError> {
    let record = store
        .qwenwork_account_record(id)
        .ok_or_else(|| GatewayError::with_status(404, "QwenWork 账号不存在"))?;
    let id = text(&record, &["id"]);
    let proxy = auth::proxy(&record)?;
    let credentials = refresh::ensure(store, &id, false).await?;
    let first = auth::get(&credentials, auth::ACCOUNT, false, proxy.as_ref()).await;
    let value = match first {
        Err(error) if error.status_code == 401 => {
            // 只有原始 401 允许一次受管理刷新重试（与余额查询同口径）
            let credentials = refresh::ensure(store, &id, true).await?;
            auth::get(&credentials, auth::ACCOUNT, false, proxy.as_ref()).await?
        }
        other => other?,
    };
    let normalized = balance::normalize(&value)?;
    let remaining = normalized
        .get("available")
        .and_then(Value::as_f64)
        .ok_or_else(|| GatewayError::with_status(502, "QwenWork 未提供可用积分读数"))?;
    let now = crate::server::logging::now_ms();
    // 对比**在写回之前**做：写回之后 last_success_at 就是今天了，判据会失效
    let credited = credited_since_last_day(&id, remaining, now);
    // 本次读数写回余额快照表：与余额查询链同一份事实（下次跨日对比的基准）
    usage_records::write_result(
        &id,
        &usage_records::UsageOutcome {
            usage: Some(normalized),
            error: None,
            code: None,
        },
    );
    Ok(DailyGiftOutcome { remaining, credited })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 三档文案与 success 位：只有**确实变多**才算今天到账。
    /// 「没变」与「判不出」都必须是中性（不落台账），不能报成功 —— 那会让用户
    /// 以为今天已经领过了。
    #[test]
    fn only_a_real_increase_counts_as_credited() {
        let up = DailyGiftOutcome { remaining: 900.0, credited: Some(200.0) }.to_claim();
        assert_eq!(up["success"], json!(true));
        assert_eq!(up["rewardPoints"], json!(200.0));
        assert!(up["msg"].as_str().unwrap().contains("已到账"));

        let flat = DailyGiftOutcome { remaining: 700.0, credited: Some(0.0) }.to_claim();
        assert_eq!(flat["success"], json!(false));
        assert!(flat["rewardPoints"].is_null(), "没变不得报奖励");
        assert!(flat["msg"].as_str().unwrap().contains("尚未发放"));

        let down = DailyGiftOutcome { remaining: 650.0, credited: Some(-50.0) }.to_claim();
        assert_eq!(down["success"], json!(false));
        assert!(down["msg"].as_str().unwrap().contains("尚未发放"));

        let unknown = DailyGiftOutcome { remaining: 700.0, credited: None }.to_claim();
        assert_eq!(unknown["success"], json!(false));
        assert!(unknown["msg"].as_str().unwrap().contains("尚未发放"));
    }

    /// 「尚未发放」必须能被 `claim_pending` 认出来（定时链据此稍后重试），
    /// 而「已到账」必须能被 `checkin_completed_today` 认出来（据此落今日台账）。
    /// 这两条判据在别处（`billing::checkin`），文案改了这里会立刻红。
    #[test]
    fn the_two_verdicts_feed_the_shared_ledger_judgements() {
        use crate::server::core::billing::checkin::{checkin_completed_today, claim_pending};
        let up = DailyGiftOutcome { remaining: 900.0, credited: Some(200.0) }.to_claim();
        assert!(checkin_completed_today(&up));
        assert!(!claim_pending(&up));
        for credited in [Some(0.0), Some(-1.0), None] {
            let flat = DailyGiftOutcome { remaining: 700.0, credited }.to_claim();
            assert!(!checkin_completed_today(&flat), "没变不得算已完成");
            assert!(claim_pending(&flat), "没变应走中性路径等重试");
        }
    }

    /// 余额读不到时不能当成 0 或「已到账」——与余额查询同一条口径
    /// （`balance::normalize` 的 `remaining: null` 是错误，不是零）。
    #[test]
    fn a_missing_reading_is_an_error_not_zero() {
        assert!(balance::normalize(&json!({"code":"ok","data":{"quota":{"remaining":null}}})).is_err());
    }
}
