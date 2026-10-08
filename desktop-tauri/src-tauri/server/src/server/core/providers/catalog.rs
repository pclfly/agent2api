//! 聚合模型目录：内置提供商的原始清单、对外绑定及发送名解析。
//!
//! 原始模型 ID 是默认绑定，别名是额外绑定。各绑定独立开关；关闭默认绑定
//! 不会禁用它指向的上游模型，也不会影响仍开启的别名。
//! 清单来源在本文件，绑定解析在 routing.rs，对外与管理视图在 view.rs。

mod routing;
mod view;

use std::collections::HashSet;

use serde_json::Value;

use crate::server::core::account_store::AccountStore;
use crate::server::core::model_rules;
use crate::server::core::models::{model_id, ModelCatalog};
use crate::server::core::providers::adapter::adapter_for;
use crate::server::core::providers::{kind_from_id, kind_id, ProviderKind, PROVIDERS};

use super::autoclaw::region::Region as AutoclawRegion;
use super::workbuddy::Region as WorkBuddyRegion;

pub use routing::{
    default_model_catalog, default_model_usable, forwarding_providers, model_blocked_everywhere,
    providers_for_model, wire_target_for_provider, WireTarget,
};
pub use view::{
    advertised_manifest_contains, advertised_model_ids, has_available_providers, manage_view,
    models_by_provider, models_response, session_models, suggest_advertised,
};

/// 原始能力清单不应用绑定开关；关闭原始 ID 后，别名仍需解析到这条上游记录。
///
/// 用户的能力位覆盖（`modelRules.capabilities`）在**这里**统一应用：本函数是
/// 内置家全部出口（`/v1/models`、Anthropic 列表、管理视图）的清单源头，收在
/// 一处就不会出现「管理页改了、下游没变」的分叉。覆盖只改元数据键，不参与
/// 路由 / 启停（见 `model_rules` 模块头的能力位覆盖一节）。
fn manifest_for(kind: ProviderKind) -> Vec<Value> {
    let mut models = adapter_for(kind).list_models();
    augment_manifest(kind, &mut models);
    models
}

/// 内置家清单的两步后处理：用户自定义模型 + 能力位覆盖。
///
/// 抽出来是因为 WorkBuddy 现在按版本出**两条**清单（见 [`manifests_for`]），
/// 两条都要走同样的后处理 —— 各写一遍就是一处会漂移的知识。
fn augment_manifest(kind: ProviderKind, models: &mut Vec<Value>) {
    for item in model_rules::custom_models_for(kind_id(kind)) {
        let id = model_id(&item);
        if !id.is_empty()
            && !models.iter().any(|existing| model_id(existing).eq_ignore_ascii_case(&id))
        {
            models.push(item);
        }
    }
    model_rules::apply_capability_overrides(kind_id(kind), models);
}

/// 一家的清单（**WorkBuddy 的两个地区各出一条**）。
///
/// ── 为什么 WorkBuddy 要按地区拆开（2026-10，参考 workbuddy2api-hub）──
/// 上游两个站点下发**两套清单**（国际版 29 个 / 国内版 55 个），交集里同名模型
/// 的倍率也不同（`fast-model` 国际版 x0.34、国内版 x0.21；`deep-model` x3.33 /
/// x1.20）。合成一条会同时踩两个坑：管理页显示「A 版本的名字 + B 版本的倍率」，
/// 且后刷新的那个版本会把另一个版本的独有模型整个挤掉（国际版的 `gpt-5.x` /
/// `grok-4.7`，国内版的 `hy3-b` / `minimax-*` 都曾这样消失）。
///
/// 拆家后两个地区是**两家独立 provider**（`workbuddy` / `workbuddy-intl`，
/// 见 `providers::workbuddy::region`），各自持有自己的目录实例与缓存槽，
/// 因此这里对每个 kind 只出一条清单；对外的同名去重顺序由注册表顺序决定
/// （国内版在前 = 与存量账号的归属一致）。
///
/// 每条仍带 `edition` 标记：管理视图按它分组、前端据此显示版本标签。
/// 两版都有的模型再补一条 `xxx-cn` / `xxx-intl` 后缀别名（见
/// [`append_edition_alias`]）—— 那是「同名模型想指定打哪一站」的路由手段，
/// 与拆家正交（对外名在两家之间本来就会撞，后缀让下游能点名单边）。
fn manifests_for(store: &AccountStore, kind: ProviderKind) -> Vec<Vec<Value>> {
    let Some(region) = WorkBuddyRegion::from_kind(kind) else {
        return vec![advertised_manifest_for(store, kind)];
    };
    let edition = region.id();
    let other = match region {
        WorkBuddyRegion::Cn => WorkBuddyRegion::Intl,
        WorkBuddyRegion::Intl => WorkBuddyRegion::Cn,
    };
    // 两版各自先收窄（别名只给**留下**的模型生成，见下），再互相取 id 集：
    // 别名只补「两版都有的真模型」。
    let curated_of =
        |region: WorkBuddyRegion| workbuddy_curated(workbuddy_catalog(region).list(), region.id());
    let ids_of = |models: &[Value]| -> std::collections::HashSet<String> {
        models
            .iter()
            .map(|item| model_id(item).to_lowercase())
            .filter(|id| !id.is_empty())
            .collect()
    };
    let other_ids = ids_of(&curated_of(other));
    let mut models = curated_of(region);
    augment_manifest(kind, &mut models);
    for item in models.iter_mut() {
        if let Some(object) = item.as_object_mut() {
            object.insert("edition".to_string(), Value::String(edition.to_string()));
        }
    }
    // 两版都有的模型补一条**本版本**的后缀别名（各自的倍率/名称都对：
    // 别名从本版清单的条目克隆，不是从另一版借的）
    append_edition_alias(&mut models, &other_ids, edition);
    vec![adapter_for(kind).advertise_models(store, models)]
}

// ─── WorkBuddy 清单收窄（与 workbuddy2api-hub 的 /v1/models 对齐）──────
//
// 上游 `/v3/config` 下发的是一份**大目录**（含虚拟档位别名、区域/变体后缀、
// 非对话模型、以及官方桌面端选择器已经不展示的旧条目）；hub 的 `/v1/models`
// 只广告「桌面端选择器里能看到的那批」：国际版 20 条、国内版 15 条（实测
// 2026-10-05，hub v1.6.10 的 `/v1/models?realm=intl|cn`）。
//
// 本家原来把大目录整份透出（国际版 27 / 国内版 37），于是同一个账号在 hub 与
// 本网关看到的模型清单不一致 —— 用户按 hub 的清单配客户端，到了这边会看到
// 一堆在官方客户端里根本不存在的名字。这里按 hub 的三条规则收窄：
//
//   ① `is_chat_model`：非对话模型（补全 / 图像 / 旧一代小输出）不要；
//   ② 变体后缀 `-sg` / `-x`：同一个名字的付费变体，选择器只留主名；
//   ③ 「有免费同族兄弟时留免费那个」：`xxx-f` 免费档在时丢掉付费的 `xxx`
//      （反之亦然）—— 判据是 `credits` 的 `x0` / `x0.00`。
//
// 顺序也照抄：**先按 hub 的白名单顺序排，再按上游顺序追加「白名单没见过但
// 上游目录里有」的 extras**（hub 的 `merge_catalog(..., extras=True)`）——
// 上游新增的模型不必等一次发版就能透出。

/// hub 的国际版顺序表（`INTL_UI_ORDER`，v1.6.10）。
const WORKBUDDY_INTL_ORDER: &[&str] = &[
    "hy4-preview-f",
    "hy3",
    "deepseek-v4.1-flash",
    "gpt-6-astra",
    "gpt-5.6-sol",
    "gpt-5.6-terra",
    "gpt-5.6-luna",
    "gpt-5.5",
    "gpt-5.4",
    "grok-4.7",
    "gemini-3.5-flash",
    "glm-5.3-flash",
    "glm-5.3",
    "glm-5.2",
    "kimi-k3",
    "kimi-k2.6",
    "kimi-k2.8-preview",
];

/// hub 的国内版顺序表（`CN_UI_ORDER`，v1.6.10）。
const WORKBUDDY_CN_ORDER: &[&str] = &[
    "hy4-preview-f",
    "hy3",
    "deepseek-v4.1-flash",
    "glm-5.3",
    "glm-5.3-flash",
    "glm-5.2",
    "glm-5.1",
    "glm-5v-turbo",
    "minimax-m3",
    "kimi-k3-1",
    "kimi-k2.8-preview",
    "kimi-k2.7",
    "kimi-k2.6",
    "deepseek-v4-pro",
];

/// hub 的 `VARIANT_SUFFIXES`：同一个名字的区域 / 变体构建。
const WORKBUDDY_VARIANT_SUFFIXES: &[&str] = &["-sg", "-x"];

/// hub 的 `NON_CHAT_MODELS`：虚拟档位别名（不是真模型）。
const WORKBUDDY_VIRTUAL_ALIASES: &[&str] = &[
    "lite",
    "auto",
    "default-model",
    "fast-model",
    "balanced-model",
    "primary-model",
    "deep-model",
];

/// hub 的 `NON_CHAT_PREFIXES` / `NON_CHAT_SUFFIXES`。
const WORKBUDDY_NON_CHAT_PREFIXES: &[&str] = &["codewise-", "completion-"];
const WORKBUDDY_NON_CHAT_SUFFIXES: &[&str] =
    &["-image-alpha", "-image-alpha-edit", "-taco-completion"];

/// 收窄一份版本清单（纯函数，便于测）：过滤 + 排序，语义见上面那段说明。
fn workbuddy_curated(models: Vec<Value>, edition: &str) -> Vec<Value> {
    let keep = |model: &Value| {
        let id = model_id(model);
        if id.is_empty() {
            return false;
        }
        let lowered = id.to_lowercase();
        if WORKBUDDY_VIRTUAL_ALIASES
            .iter()
            .any(|alias| lowered == *alias)
        {
            return false;
        }
        if WORKBUDDY_NON_CHAT_PREFIXES
            .iter()
            .any(|prefix| lowered.starts_with(prefix))
        {
            return false;
        }
        if WORKBUDDY_NON_CHAT_SUFFIXES
            .iter()
            .any(|suffix| lowered.ends_with(suffix))
        {
            return false;
        }
        if WORKBUDDY_VARIANT_SUFFIXES
            .iter()
            .any(|suffix| lowered.ends_with(suffix))
        {
            return false;
        }
        true
    };
    let kept: Vec<Value> = models.into_iter().filter(keep).collect();
    // 免费档判据：`credits` 恰为 `x0.00` / `x0` / `0` / `0.00`（hub 的
    // `snapshot_credits()` 同口径，**不做数值解析**）。缺 `credits` 一律算
    // 「不是免费档」—— 与 hub 把缺失读成空串同义。
    let free = |model: &Value| {
        matches!(
            model.get("credits").and_then(Value::as_str).map(str::trim),
            Some("x0.00" | "x0" | "0" | "0.00")
        )
    };
    let free_id_set: HashSet<String> = kept
        .iter()
        .filter(|model| free(model))
        .map(|model| model_id(model).to_lowercase())
        .collect();
    let mut selected: Vec<Value> = Vec::with_capacity(kept.len());
    for model in kept {
        let id = model_id(&model).to_lowercase();
        // 「有免费同族兄弟时留免费那个」（hub 的 `curate_remote_catalog` 同序）：
        //   · `xxx-f` 免费 → 丢掉付费的 `xxx`（保留 `-f`）；
        //   · `xxx-f` 付费而 `xxx` 免费 → 丢掉付费的 `-f`（保留基名）。
        // 方向不能反：`hy4-preview`(x0.29) / `hy4-preview-f`(x0.00) 这一对里，
        // hub 留的是 `hy4-preview-f` —— 反了就会与 hub 差一条（实测踩过）。
        let is_free = free_id_set.contains(&id);
        let paid = if let Some(base) = id.strip_suffix("-f") {
            free_id_set.iter().any(|known| known == base)
        } else {
            free_id_set.iter().any(|known| known == &format!("{id}-f"))
        } && !is_free;
        if paid {
            continue;
        }
        selected.push(model);
    }
    let order: &[&str] = if edition == crate::server::core::models::EDITION_INTL {
        WORKBUDDY_INTL_ORDER
    } else {
        WORKBUDDY_CN_ORDER
    };
    let mut out: Vec<Value> = Vec::new();
    let mut used: Vec<String> = Vec::new();
    // ① 白名单顺序：清单里有、且命中白名单的，按白名单排
    for wanted in order {
        if let Some(model) = selected
            .iter()
            .find(|model| model_id(model).eq_ignore_ascii_case(wanted))
        {
            used.push(model_id(model).to_lowercase());
            out.push(model.clone());
        }
    }
    // ② extras：白名单没见过、但清单里有的，按清单原顺序追加（hub 同口径）
    for model in selected {
        let id = model_id(&model).to_lowercase();
        if used.contains(&id) {
            continue;
        }
        used.push(id);
        out.push(model);
    }
    out
}

/// 给本版清单里「两版都有」的模型补一条 `xxx-intl` / `xxx-cn` 别名。
///
/// ── 为什么只给两版都有的加 ──────────────────────────────────
/// 独有模型（`hy3-b` 只在国内版有）本来就唯一，加后缀只是噪音；而
/// `deepseek-v4.1-flash` 这种两边都有的（国际版 x0.00 限时免费 / 国内版
/// x0.11），下游无法从名字上区分要哪个版本 —— 后缀就是那个区分手段：
///   - `deepseek-v4.1-flash-intl` → 固定走国际版账号；
///   - `deepseek-v4.1-flash-cn`   → 固定走国内版账号；
///   - 不带后缀的那条保持原语义（走队首账号所属的版本）。
///
/// 每个版本只补**自己那一条**，于是对外合并时两条都在、各自的倍率来自各自
/// 的清单（从另一版借条目会让 `-cn` 显示成国际版的倍率）。
/// 解析在 `models::split_edition_suffix`（路由解析与发送侧共用一处）。
///
/// 拆家后两个地区已是两家独立 provider：不带后缀的名字由 `forwarding_providers`
/// 按各家清单分别命中（两个地区都会进候选链），带后缀的名字只命中对应的那一家
/// —— 后缀因此从「选路时收窄账号」变成「按名字直接点名单边」，语义更直白。
///
/// ⚠️ 生成的别名带 `editionAliasOf` 标记 —— 管理页的模型列表据此把它排除
/// （它不是上游模型，不该占一行；见 `catalog::view::manage_view`）。
fn append_edition_alias(
    models: &mut Vec<Value>,
    other_ids: &std::collections::HashSet<String>,
    edition: &str,
) {
    use crate::server::core::models::{
        edition_label, shape_value_text, EDITION_INTL, EDITION_SUFFIX_CN, EDITION_SUFFIX_INTL,
    };
    let suffix = if edition == EDITION_INTL {
        EDITION_SUFFIX_INTL
    } else {
        EDITION_SUFFIX_CN
    };
    let mut extra: Vec<Value> = Vec::new();
    for item in models.iter() {
        let id = model_id(item);
        if id.is_empty() || !other_ids.contains(&id.to_lowercase()) {
            continue;
        }
        let name = item
            .get("name")
            .map(shape_value_text)
            .unwrap_or_else(|| id.clone());
        let mut alias = item.clone();
        if let Some(object) = alias.as_object_mut() {
            object.insert("id".to_string(), Value::String(format!("{id}{suffix}")));
            object.insert(
                "name".to_string(),
                Value::String(format!("{name}（{}）", edition_label(edition))),
            );
            object.insert("edition".to_string(), Value::String(edition.to_string()));
            object.insert("editionAliasOf".to_string(), Value::String(id.clone()));
        }
        extra.push(alias);
    }
    models.extend(extra);
}

fn advertised_manifest_for(store: &AccountStore, kind: ProviderKind) -> Vec<Value> {
    adapter_for(kind).advertise_models(store, manifest_for(kind))
}

fn workbuddy_catalog(region: WorkBuddyRegion) -> ModelCatalog {
    crate::server::core::models::global_catalog(region)
}

fn autoclaw_catalog_state(region: AutoclawRegion) -> (bool, i64) {
    (
        !super::autoclaw::catalog::remote_models(region).is_empty(),
        super::autoclaw::catalog::last_refreshed_at(region),
    )
}

/// 这一家清单的「远程与否 + 拉取时刻」。
///
/// 两个消费方：本模块的对外视图（`meta.source` / 管理页的「来源」列）与
/// **适配器层的手动刷新结果**（`adapter::refresh_implemented_forced` 的
/// `refreshedAt`，界面「更新日期」列读它）—— 后者必须走这里而不是自己按
/// kind 拼一遍：十家的取值路径（workbuddy 按地区分格、按地区分格的三家、
/// Cline 的池）各不相同，抄一份就是一处会漂移的知识。
pub(crate) fn refresh_meta(kind: ProviderKind) -> (bool, i64) {
    match kind {
        // WorkBuddy 的两个地区各有自己的目录实例与缓存槽（见
        // `providers::workbuddy::region` 的模块头）：这一处也是两家分开的
        // 语义来源之一 —— 界面「更新日期」列因此如实显示各自那次拉取。
        ProviderKind::WorkBuddy => {
            let catalog = workbuddy_catalog(WorkBuddyRegion::Cn);
            (catalog.remote_refreshed(), catalog.last_refreshed_at())
        }
        ProviderKind::WorkBuddyIntl => {
            let catalog = workbuddy_catalog(WorkBuddyRegion::Intl);
            (catalog.remote_refreshed(), catalog.last_refreshed_at())
        }
        ProviderKind::Raccoon => (
            super::raccoon::models::remote_refreshed(),
            super::raccoon::models::last_refreshed_at(),
        ),
        ProviderKind::Qoder => (
            super::qoder::models::remote_refreshed(super::qoder::endpoints::Region::Global)
                || super::qoder::models::remote_refreshed(super::qoder::endpoints::Region::Cn),
            super::qoder::models::last_refreshed_at(),
        ),
        ProviderKind::CodeArts => (
            super::codearts::models::remote_refreshed(),
            super::codearts::models::last_refreshed_at(),
        ),
        ProviderKind::CatPaw => (
            !super::catpaw::catalog::remote_models().is_empty(),
            super::catpaw::catalog::last_refreshed_at(),
        ),
        ProviderKind::AutoClaw => autoclaw_catalog_state(AutoclawRegion::Cn),
        ProviderKind::AutoClawIntl => autoclaw_catalog_state(AutoclawRegion::Intl),
        ProviderKind::ClineFree | ProviderKind::ClinePass => (
            super::cline::models::remote_refreshed(),
            super::cline::models::last_refreshed_at(),
        ),
        // Accio 两个地区各有自己的目录缓存（上游按 `x-package-region` 给清单）：
        // 两家任一刷过就算「有远程来源」，时间取两者里更近的那次
        ProviderKind::Accio | ProviderKind::AccioCn => {
            let region = super::accio::endpoints::Region::from_kind(kind)
                .unwrap_or(super::accio::endpoints::Region::Global);
            (
                !super::accio::models::remote_models(region).is_empty(),
                super::accio::models::last_refreshed_at(region),
            )
        }
        // ZCode 两个地区共用一份**静态**清单（上游没有列模型的公开接口，
        // 见 `zcode::models` 的模块头）：永远不是远程来源，也没有刷新时刻。
        // 这里如实回 `(false, 0)` 而不是编一个时间 —— 界面的「来源」列会显示成
        // 内置清单，与事实相符。
        ProviderKind::Zcode | ProviderKind::ZcodeIntl => (false, 0),
        // Trae 只有一张表（SOLO 通道），刷过就是远程来源；没刷到时是**空清单**
        // 而不是静态兜底 —— 目录由 `X-Ide-Version-Code` 决定给哪张表，抄成
        // 常量就是把一个时间点的读数当契约（见 `trae::models` 模块头）。
        ProviderKind::Trae => (
            super::trae::models::remote_refreshed(),
            super::trae::models::last_refreshed_at(),
        ),
        // LobsterAI：远程目录已接真身（刷过就是远程来源）。
        ProviderKind::LobsterAi => (
            super::lobsterai::models::remote_refreshed(),
            super::lobsterai::models::last_refreshed_at(),
        ),
        // Loomy 的清单来自 `GET {集成网关}/api/v1/models`（OpenAI 格式）。
        // 上游**没有**内置兜底清单，所以「有内容」就等于「远程拉到过」。
        ProviderKind::Loomy => (
            super::loomy::models::remote_refreshed(),
            super::loomy::models::last_refreshed_at(),
        ),
        // KukuAI 的清单来自远程目录（`/wenchain/genflowpro/model_list`）；
        // 静态兜底只是离线保底，「有内容」才算远程来源（与 raccoon 同判据）。
        ProviderKind::Kuku => (
            super::kuku::models::remote_refreshed(),
            super::kuku::models::last_refreshed_at(),
        ),
        // MiniMax：远程目录已接真身（刷过就是远程来源）。
        ProviderKind::Minimax => (
            super::minimax::models::remote_refreshed(),
            super::minimax::models::last_refreshed_at(),
        ),
    }
}

fn all_kinds() -> Vec<ProviderKind> {
    PROVIDERS.iter().filter_map(|meta| kind_from_id(meta.id)).collect()
}

pub fn provider_available(store: &AccountStore, kind: ProviderKind) -> bool {
    !store.accounts_for_provider(kind_id(kind)).is_empty()
        || adapter_for(kind).env_credentials_present()
}

/// 管理视图需要完整清单；对外视图在此基础上再应用每条绑定的开关。
///
/// 返回的是 **(provider, 清单)** 列表 —— 拆家后每个 kind 只贡献一条
/// （WorkBuddy 的两个地区已是两家 provider，各自一条；见 [`manifests_for`]）。
pub fn active_manifests(store: &AccountStore) -> Vec<(ProviderKind, Vec<Value>)> {
    all_kinds()
        .into_iter()
        .filter(|kind| provider_available(store, *kind))
        .flat_map(|kind| {
            manifests_for(store, kind)
                .into_iter()
                .filter(|manifest| !manifest.is_empty())
                .map(move |manifest| (kind, manifest))
        })
        .collect()
}

fn aggregate_source(active: &[(ProviderKind, Vec<Value>)]) -> (&'static str, i64) {
    let Some((first, _)) = active.first() else {
        return ("none", 0);
    };
    // 只有一家时仍按这一家的来源算，否则单家用户的 `meta.source` 会从
    // `remote` 变成没头没尾的 `aggregate`（WorkBuddy 的两个地区是两家
    // provider，因此「只加了国内版账号」时这里仍报 remote/builtin 而非 aggregate）
    if active.iter().all(|(kind, _)| kind == first) {
        let (remote, refreshed_at) = refresh_meta(*first);
        return (if remote { "remote" } else { "builtin" }, refreshed_at);
    }
    ("aggregate", refresh_meta(*first).1)
}

#[cfg(test)]
mod tests {
    //! WorkBuddy 清单收窄（与 workbuddy2api-hub 的 `/v1/models` 对齐）的回归测试。
    //!
    //! 守的是「同一个账号在 hub 与本网关看到的清单不一致」这个症状：大目录里的
    //! 虚拟档位别名、`-sg` / `-x` 变体、`xxx` 与免费 `xxx-f` 的同族重复、以及
    //! 官方选择器已不展示的旧条目，都不该出现在本网关的对外/管理清单里。
    use super::*;
    use crate::server::core::models::{EDITION_CN, EDITION_INTL};

    fn model(id: &str, credits: Option<&str>) -> Value {
        let mut item = serde_json::json!({ "id": id, "name": id });
        if let Some(credits) = credits {
            item["credits"] = Value::String(credits.to_string());
        }
        item
    }

    fn ids(models: &[Value]) -> Vec<String> {
        models.iter().map(model_id).collect()
    }

    #[test]
    fn virtual_aliases_and_variant_builds_are_dropped() {
        let curated = workbuddy_curated(
            vec![
                model("auto", None),
                model("fast-model", Some("x0.21")),
                model("deep-model", Some("x1.20")),
                model("deepseek-v4.1-flash-sg", Some("x0.03")),
                model("hy3-x", Some("x0.05")),
                model("codewise-1", None),
                model("completion-gf", None),
                model("kimi-k2.6", Some("x0.52")),
            ],
            EDITION_CN,
        );
        assert_eq!(vec!["kimi-k2.6"], ids(&curated));
    }

    #[test]
    fn free_sibling_displaces_the_paid_one() {
        // 真实那一对：`hy4-preview`(x0.29) / `hy4-preview-f`(x0.00) —— 留免费那条。
        let intl = workbuddy_curated(
            vec![
                model("hy4-preview", Some("x0.29")),
                model("hy4-preview-f", Some("x0.00")),
            ],
            EDITION_INTL,
        );
        assert_eq!(vec!["hy4-preview-f"], ids(&intl));
        // 反过来：基名免费、`-f` 付费 → 丢 `-f`、留基名。
        let flipped = workbuddy_curated(
            vec![
                model("deepseek-v4.1-flash", Some("x0.00")),
                model("deepseek-v4.1-flash-f", Some("x0.11")),
            ],
            EDITION_CN,
        );
        assert_eq!(vec!["deepseek-v4.1-flash"], ids(&flipped));
        // 没有免费兄弟时付费那条必须留着。
        let paid_only = workbuddy_curated(vec![model("glm-5.1", Some("x0.79"))], EDITION_CN);
        assert_eq!(vec!["glm-5.1"], ids(&paid_only));
    }

    #[test]
    fn curated_order_table_decides_the_order_and_extras_come_last() {
        // 白名单没见过的上游新模型（extras）排在白名单之后，且顺序不变。
        let curated = workbuddy_curated(
            vec![
                model("space-bunny", Some("x0.03")),
                model("glm-5.2", Some("x0.79")),
                model("hy3", Some("x0.00")),
            ],
            EDITION_CN,
        );
        assert_eq!(vec!["hy3", "glm-5.2", "space-bunny"], ids(&curated));
    }

    #[test]
    fn the_two_editions_use_their_own_order_table() {
        // 同一个 id 在两版的相对位置不同（`gpt-5.6-sol` 只在国际版白名单里）。
        let intl = workbuddy_curated(
            vec![
                model("gpt-6-astra", Some("x6.67")),
                model("gpt-5.6-sol", Some("x3.47")),
                model("hy3", Some("x0.00")),
            ],
            EDITION_INTL,
        );
        assert_eq!(vec!["hy3", "gpt-6-astra", "gpt-5.6-sol"], ids(&intl));
        // 国内版白名单不认识 `gpt-5.6-sol`，它落到 extras 末尾。
        let cn = workbuddy_curated(
            vec![
                model("gpt-6-astra", Some("x6.67")),
                model("gpt-5.6-sol", Some("x3.47")),
                model("hy3", Some("x0.00")),
            ],
            EDITION_CN,
        );
        assert_eq!(vec!["hy3", "gpt-6-astra", "gpt-5.6-sol"], ids(&cn));
    }

    #[test]
    fn an_empty_or_fully_filtered_catalog_stays_empty() {
        assert!(workbuddy_curated(Vec::new(), EDITION_CN).is_empty());
        assert!(workbuddy_curated(vec![model("auto", None)], EDITION_INTL).is_empty());
        // 没有 id 的条目整条丢弃（它进不了 /v1/models）
        assert!(workbuddy_curated(vec![serde_json::json!({ "name": "无 id" })], EDITION_CN).is_empty());
    }

    /// 版本后缀别名只给「两版都有」的模型生成，且带 `editionAliasOf` 标记 ——
    /// 管理页的模型列表靠这个标记把它排除（否则条数会比真模型多出一截）。
    #[test]
    fn edition_aliases_are_marked_and_only_for_shared_models() {
        let shared: std::collections::HashSet<String> =
            ["hy3".to_string(), "glm-5.3".to_string()].into_iter().collect();
        let mut models = vec![model("hy3", Some("x0.00")), model("space-bunny", Some("x0.03"))];
        append_edition_alias(&mut models, &shared, EDITION_CN);
        assert_eq!(vec!["hy3", "space-bunny", "hy3-cn"], ids(&models));
        let alias = models.last().expect("别名应当在末尾");
        assert_eq!(Some("hy3"), alias.get("editionAliasOf").and_then(Value::as_str));
        // 原条目不带这个标记（管理页据此区分「真模型」与「别名」）
        assert!(models[0].get("editionAliasOf").is_none());
        // 国际版用另一个后缀
        let mut intl = vec![model("glm-5.3", Some("x0.79"))];
        append_edition_alias(&mut intl, &shared, EDITION_INTL);
        assert_eq!(vec!["glm-5.3", "glm-5.3-intl"], ids(&intl));
    }
}
