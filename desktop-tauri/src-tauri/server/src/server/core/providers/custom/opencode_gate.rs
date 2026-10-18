//! OpenCode 免费车道的**出站节奏控制**：门禁成因判定、通道冷却、出口并发闸
//! （2026-10-18 实测驱动）。
//!
//! ── 为什么需要这个模块 ──────────────────────────────────────
//! 改造前，`forward.rs` 把上游 401/403 一律归成「凭证被拒绝」。对 OpenCode
//! 免费档这是**三条完全不同的故障被压成一句话**，用户看到的提示（「上游凭证
//! 被拒绝，检查 apiKey」）与真实原因毫无关系：
//!
//! ```text
//! 403 FreeTierError: OpenCode's free tier can only be used from within OpenCode
//!     → 客户端形态门禁。改指纹/工具/会话头**能修**。
//! 403 FreeTierError: … not available in your country
//!     → 地区封锁。改任何请求头都**修不了**，只能换出口。
//! 429 FreeUsageLimitError（或 403 携带该类型码）
//!     → 该**出口地址**的额度耗尽。指纹修不了，只能换出口或等。
//! 400/403 提到 model 不可用
//!     → 这个模型不在免费档的服务范围里。换模型，跟指纹与出口都无关。
//! ```
//!
//! 前三条的共同点是类型码都叫 `FreeTierError` —— 只看类型码必然误判。这也是
//! 社区里「同一时刻有人说全灭、有人说已过闸」的根因：他们各自撞的是不同那一条。
//!
//! ── 两条通道，两套门禁 ──────────────────────────────────────
//! 2026-10-18 实测：`/zen/v1`（legacy）与 `/inference/v1` **同时可用**，但门禁
//! 规格不同（zen 要 `x-opencode-request` + `prj_` 形 project + 只补一个 `read`；
//! inference 禁发 request 头、要 `bash`+`read`）。所以「通道」是判定与冷却的
//! 最小单位，冷却也必须**按通道**记 —— 一条通道被标记时另一条还能用。
//!
//! ── 冷却为什么有两个作用域 ──────────────────────────────────
//! 三条成因的**可修路径不同**，作用域必须跟着不同：
//!
//!   - [`Scope::Provider`]（形态门禁）：看的是「请求长什么样」，与从哪个地址
//!     出去无关 —— 换出口重试是白花往返，所以冷却盖住整个 provider；
//!   - [`Scope::Egress`]（地区封锁 / 额度耗尽）：看的是「从哪个地址出去」，
//!     换出口**就是**唯一的解法 —— 所以冷却只钉住那一个出口。这一条不是洁癖：
//!     `provider_loop::attempt_custom` 对轮询档的 429 会原地换出口重发，若把
//!     额度冷却记成 provider 级，第二次重发会被自己的冷却挡住，等于把既有的
//!     补救机制废掉。
//!
//! 因此键是 `(provider_id, lane, scope)`：与 `upstream::route_state` 的账号级
//! 状态正交（那边记「这个账号/模型还能不能打」，这边记「这条通道/这个出口
//! 现在能不能打」）。
//!
//! ── 出口并发闸：冷却之外的第二道「防封」────────────────────
//! 冷却管的是**被拒之后**，并发闸管的是**被拒之前**。社区实测（OpenCode2api）：
//! 同一个出口连发 10 个 1MB 级请求就会触发 IP 级标记，之后三个模型全 403，
//! 且 600s 冷却也不一定恢复 —— 也就是说**冲过头一次，冷却也救不回来**。
//!
//! 所以每个出口压一个在飞上限（[`MAX_IN_FLIGHT_PER_EGRESS`]，默认 4）。三条
//! 口径值得写下来：
//!
//!   - **按出口计，不按 provider 计**：被标记的是 IP，两个 provider 走同一个
//!     代理就是在共用同一份预算。键与冷却的 `Scope::Egress` 同源（[`egress_key`]
//!     的产物），直连是空串（本机地址也是一个出口）；
//!   - **只闸 OpenCode 免费档**：别家上游没有这套 IP 级风控，给它们限流是纯粹
//!     的性能损失。调用方用 `lane.is_some()` 作开关；
//!   - **许可在「响应头到手」时归还**（流式）／「聚合完成」时归还（非流式）：
//!     不持有整条流的生命周期。理由是 IP 级标记发生在**建连到首字节**这段密集
//!     期，而长流串行化会把网关的并发吞吐直接砍到 4 —— 那是用错了药。代价是
//!     这个上限约束的是「同时在飞的上游握手数」，不是「同时在跑的长连接数」，
//!     这一条必须诚实写在文档里，别让人以为它是硬并发上限。
//!
//! ── 硬约束 ──────────────────────────────────────────────────
//! release 是 `panic=abort`：本模块零 unwrap/expect；锁中毒按「无冷却 / 不限流」
//! 处理（宁可多打一次上游，也不要因为一次 panic 把转发永久堵死）。

use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock};

use tokio::sync::{OwnedSemaphorePermit, Semaphore};

use crate::server::core::custom_providers;
use crate::server::core::proxies::ResolvedProxy;

/// 门禁拒绝后的冷却时长（与 OpenCode2api 的 `UPSTREAM_COOLDOWN` 同量级）。
///
/// 600s 的理由是实测：被标记后继续撞只会续期，而 10 分钟足够上游把一次性
/// 风控计数衰减掉。太短等于没冷却，太长会让一次误判把用户锁十分钟。
pub const GATE_COOLDOWN_MS: i64 = 600_000;

/// 429（额度耗尽）后的冷却时长：短冷却防硬撞，429 本身仍原样透传给客户端。
pub const QUOTA_COOLDOWN_MS: i64 = 60_000;

/// OpenCode 免费档的**通道**（按端点区分，两套门禁规格不同）。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Lane {
    /// legacy `/zen/v1`：要 `x-opencode-request`、`prj_` 形 project、只补 `read`
    Zen,
    /// `/inference/v1`：禁发 request 头、要 `bash` + `read`、UA 2.x 形更稳
    Inference,
}

impl Lane {
    /// 冷却表与日志里的稳定标识（不要本地化，它出现在诊断文案里）
    pub fn id(self) -> &'static str {
        match self {
            Self::Zen => "zen",
            Self::Inference => "inference",
        }
    }

    /// 用户可读的通道名（带端点，便于照着改配置）
    pub fn label(self) -> &'static str {
        match self {
            Self::Zen => "zen（/zen/v1）",
            Self::Inference => "inference（/inference/v1）",
        }
    }

    /// 另一条通道（跨通道降级重试的落点）。
    ///
    /// 两条通道**规格不同但能力等价**（2026-10-18 实测都返回 85 个模型），
    /// 所以一条被门禁否掉时换另一条是有意义的：被否的是「这套指纹」，不是
    /// 「这个账号」。
    pub fn other(self) -> Self {
        match self {
            Self::Zen => Self::Inference,
            Self::Inference => Self::Zen,
        }
    }

    /// 该通道的规范基址（降级重试时用来替换记录里的 baseUrl）。
    pub fn base_url(self) -> &'static str {
        match self {
            Self::Zen => custom_providers::LEGACY_OPENCODE_BASE_URL,
            Self::Inference => custom_providers::OPENCODE_BASE_URL,
        }
    }
}

/// 按 baseUrl 判定通道。非 OpenCode 端点（含 Go 面）返回 None ——
/// 调用方只在 `clientEmulation` 打开时才问，这里不重复判开关。
///
/// Go 面（`/zen/go/v1`）刻意不在这里：它的要求是「一个稳定会话头」，
/// 既没有免费档门禁也没有冷却语义（见 `emulation` 的 Go 面一节）。
pub fn lane_of(base_url: &str) -> Option<Lane> {
    let url = base_url.trim().trim_end_matches('/');
    if url.eq_ignore_ascii_case(custom_providers::LEGACY_OPENCODE_BASE_URL) {
        return Some(Lane::Zen);
    }
    if url.eq_ignore_ascii_case(custom_providers::OPENCODE_BASE_URL) {
        return Some(Lane::Inference);
    }
    None
}

/// 403 / 429 / 400 的四种成因（判定依据是**错误文案**，不是类型码）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Rejection {
    /// 客户端形态门禁：改指纹、工具、会话头**能修**
    Gate,
    /// 地区封锁：任何请求头都修不了，只能换出口
    Region,
    /// 该出口地址的额度耗尽：指纹修不了，只能换出口或等
    Quota,
    /// 这个模型不在免费档的服务范围里：换模型，与指纹、出口都无关
    Model,
}

impl Rejection {
    /// 该成因的冷却作用域是不是「按出口」。
    ///
    /// 形态门禁与出口无关；地区与额度都是出口属性 —— 这一条直接决定了
    /// `attempt_custom` 的换出口重试会不会被自己的冷却挡住。
    pub fn is_egress_scoped(self) -> bool {
        match self {
            Self::Gate | Self::Model => false,
            Self::Region | Self::Quota => true,
        }
    }

    /// 该成因的冷却时长；`None` = **不冷却**。
    ///
    /// `Model` 是唯一不冷却的一条：它记在 provider 级会连带封掉这家**所有**
    /// 模型（一个错模型 id 把整家锁十分钟，那是比不冷却更糟的故障），记在
    /// 出口级又毫无意义（换出口也还是同一个模型）。客户端改一下模型名就能过，
    /// 没有需要保护的上游资源。
    pub fn cooldown_ms(self) -> Option<i64> {
        match self {
            Self::Gate | Self::Region => Some(GATE_COOLDOWN_MS),
            Self::Quota => Some(QUOTA_COOLDOWN_MS),
            Self::Model => None,
        }
    }
}

/// 从上游错误体判定成因。
///
/// 顺序是刻意的：**先认最具体的文案**，最后才落到类型码。`FreeTierError`
/// 这个类型码前三种成因共用，拿它当判据必然把地区封锁误报成「指纹不对」，
/// 那正是用户最容易白折腾一整天的一条。
///
/// `Model` 排在 `Region` 之后是必须的：`"… not available in your country"`
/// 同时满足「提到 model」与「提到 not available」，先判地区才不会被抢走。
pub fn classify(raw: &str) -> Rejection {
    let text = raw.to_ascii_lowercase();
    if text.contains("not available in your country")
        || text.contains("not available in your region")
    {
        return Rejection::Region;
    }
    if text.contains("can only be used from within opencode") {
        return Rejection::Gate;
    }
    if text.contains("freeusagelimiterror")
        || text.contains("free usage limit")
        || text.contains("usage limit reached")
    {
        return Rejection::Quota;
    }
    // 模型不在免费档的服务范围里。判据是「同时提到 model 与不可用」——
    // 上游这条的原文有多种写法（`model not found` / `unsupported model` /
    // `model … is not available`），认词根比拼死一种写法稳。
    let mentions_model = text.contains("model");
    let unavailable = text.contains("not found")
        || text.contains("not available")
        || text.contains("does not exist")
        || text.contains("unsupported")
        || text.contains("unknown model")
        || text.contains("no such model");
    if mentions_model && unavailable {
        return Rejection::Model;
    }
    // 兜底：仍带 FreeTierError 的，按门禁处理（最可修的那一种）。
    Rejection::Gate
}

/// 判定结果 → 面向用户的错误文案。
///
/// 每条都写清「该改什么」与「改了也没用的是什么」，因为四条的处置方式
/// 完全不同：门禁要改指纹，地区要换出口，额度要等或换出口，模型要换模型名。
pub fn rejection_message(lane: Lane, rejection: Rejection, raw: &str) -> String {
    let detail = raw.trim();
    let detail = if detail.is_empty() {
        "（上游未给出原因）"
    } else {
        detail
    };
    match rejection {
        Rejection::Gate => format!(
            "OpenCode 免费档门禁拒绝了这次请求（通道 {}）：请求形态未被接受，\
             通常是会话头 / 工具声明 / UA 形态不对。上游原文: {detail}",
            lane.label()
        ),
        Rejection::Region => format!(
            "OpenCode 按地区拒绝了这次请求（通道 {}）：这个出口地址所在地区不可用，\
             改任何请求头都无效，只能换出口（代理）或换网络。上游原文: {detail}",
            lane.label()
        ),
        Rejection::Quota => format!(
            "OpenCode 免费额度已耗尽（通道 {}）：配额按出口地址计，\
             与账号无关；只能换出口或等额度恢复。上游原文: {detail}",
            lane.label()
        ),
        Rejection::Model => format!(
            "这个模型不在 OpenCode 免费档的服务范围里（通道 {}）：免费档只服务\
             一部分模型（`-free` 结尾的那些，外加少数特例）。换一个免费模型即可 ——\
             换出口、改指纹、改 apiKey 都没有用。上游原文: {detail}",
            lane.label()
        ),
    }
}

/// 冷却期内的快速失败文案（不发上游，直接把节奏交还客户端）。
pub fn cooldown_message(lane: Lane, remaining_ms: i64) -> String {
    let seconds = (remaining_ms.max(0) + 999) / 1000;
    format!(
        "OpenCode 通道 {} 正在冷却（剩 {seconds}s）：上游刚拒绝过这条通道，\
         冷却期内不再请求它。可稍后重试，或改用另一条通道 / 换出口。",
        lane.label()
    )
}

/// 出口的稳定标识（冷却的「按出口」档与并发闸**共用同一个键**）。
///
/// 用 `host:port` 而不是 `label`：轮询档换出口时换的是**地址**，两条候选
/// 共用同一个 label 的情况完全可能存在（同一个机场的两个节点）。直连给空串
/// —— 直连也是一个「出口」（本机地址），它同样会被按地区/额度拒绝，也同样
/// 受并发闸约束。
///
/// 冷却与并发闸必须同源：冷却按 A 出口记、并发闸按 B 出口算，就会出现
/// 「A 出口在冷却但它的槽位还被占着」这种自相矛盾的状态。
pub fn egress_key(proxy: Option<&ResolvedProxy>) -> String {
    match proxy {
        Some(proxy) => match proxy.port {
            Some(port) => format!("{}:{port}", proxy.host.trim().to_ascii_lowercase()),
            None => proxy.host.trim().to_ascii_lowercase(),
        },
        None => String::new(),
    }
}

/// 出口的可读描述（只用于文案；空键 = 直连）。
pub fn egress_label(egress: &str) -> &str {
    if egress.is_empty() {
        "直连"
    } else {
        egress
    }
}

/// 冷却的作用域（内部形态，只在表键里出现）。
#[derive(Clone, PartialEq, Eq, Hash)]
enum Scope {
    /// 与出口无关（形态门禁）
    Provider,
    /// 按出口；空串表示直连（没有代理时，「出口」就是本机地址）
    Egress(String),
}

/// 一条冷却记录：截止时刻 + 该以什么状态码快速失败。
///
/// 状态码跟着记录走而不是跟着查询走：额度冷却要以 429 快速失败（那样
/// `attempt_custom` 才会继续轮询换出口），门禁冷却不能是 429（换出口没用，
/// 只会白花往返）。
struct Gate {
    until: i64,
    status: i32,
}

/// 冷却表：`(provider_id, lane, scope) → Gate`。
///
/// 进程内存、不落盘：这是「接下来这一会儿怎么打上游」的运行期状态，重启清零
/// 是正确语义（与 `upstream::route_state` 同口径）。真正要跨重启保留的是账号
/// 记录里的 `rateLimits`，那是另一套机制。
static GATES: OnceLock<Mutex<HashMap<(String, Lane, Scope), Gate>>> = OnceLock::new();

fn table() -> &'static Mutex<HashMap<(String, Lane, Scope), Gate>> {
    GATES.get_or_init(|| Mutex::new(HashMap::new()))
}

/// 该通道还要冷却多少毫秒、以及该用哪个状态码拒绝（None = 未冷却）。
///
/// `egress` 是调用方给出的出口标识（`None` 表示直连）。查询会同时看
/// 「provider 级」与「本出口」两条记录，取剩余时间更长的那个 —— 门禁盖住
/// 整家，地区/额度只钉住一个出口，两者并存时以更严的为准。
pub fn remaining_ms(provider_id: &str, lane: Lane, egress: Option<&str>) -> Option<(i64, i32)> {
    let now = crate::server::logging::now_ms();
    let direct = egress.unwrap_or("").to_ascii_lowercase();
    let Ok(map) = table().lock() else { return None };
    let key_egress = Scope::Egress(direct);
    [
        map.get(&(provider_id.to_string(), lane, Scope::Provider)),
        map.get(&(provider_id.to_string(), lane, key_egress)),
    ]
    .into_iter()
    .flatten()
    .map(|gate| ((gate.until - now).max(0), gate.status))
    .filter(|(remaining, _)| *remaining > 0)
    .max_by_key(|(remaining, _)| *remaining)
}

/// 标记该通道冷却 `ms` 毫秒，并以 `status` 快速失败。
///
/// `egress` 为 `None` 时记成 provider 级（形态门禁），`Some(出口)` 时只钉住
/// 那一个出口（地区封锁 / 额度耗尽）。顺手清掉已过期的条目 —— 冷却表只在
/// 「被拒绝」时写入，条目数天然有界（provider × 通道 × 出口），但仍不该无限增长。
pub fn mark(
    provider_id: &str,
    lane: Lane,
    egress: Option<&str>,
    ms: i64,
    status: i32,
) {
    let now = crate::server::logging::now_ms();
    let scope = match egress {
        Some(value) => Scope::Egress(value.to_ascii_lowercase()),
        None => Scope::Provider,
    };
    let Ok(mut map) = table().lock() else { return };
    map.retain(|_, gate| gate.until > now);
    map.insert(
        (provider_id.to_string(), lane, scope),
        Gate {
            until: now + ms.max(0),
            status,
        },
    );
}

/// 成功一次即解除该通道**全部**冷却（provider 级与本出口级都清）。
///
/// 与 `rateLimits` 的「成功清除」同一取向：上游恢复后不该继续按旧标记惩罚。
/// 只清本出口的记录会让「换了出口才成功」这种情形把 provider 级门禁永久留着，
/// 所以这里按 (provider, lane) 整体清。
pub fn clear(provider_id: &str, lane: Lane) {
    let Ok(mut map) = table().lock() else { return };
    map.retain(|(id, other, _), _| !(id == provider_id && *other == lane));
}

// ── 出口并发闸 ────────────────────────────────────────────────────────

/// 每个出口同时最多几个上游请求「在飞」。
///
/// 4 这个数字来自社区实测的反面：同一个出口连发 10 个 1MB 级请求就触发了
/// IP 级标记，之后三个模型全 403。4 是「留足日常并发（一次对话通常 1～2 路，
/// 加一路后台标题生成）又不至于把上游的计数打爆」的折中。
///
/// **刻意不做成可配置项**：它调错了的后果是「账号被标记」（要等冷却，甚至
/// 等不到），而不是「慢一点」—— 这种参数不该交给用户在界面上随手拖。
/// 真要改，改这个常量、重新编译。
pub const MAX_IN_FLIGHT_PER_EGRESS: usize = 4;

/// 等不到槽位时最多等多久。
///
/// 超时给 503 而不是无限等：无限等会把请求堆在网关里，客户端看到的只是
/// 「卡住」，既不比 503 友好，还会把内存与连接都占住。
pub const SLOT_WAIT_MS: u64 = 30_000;

/// 每个出口一把信号量。键与冷却的 `Scope::Egress` **同源**（[`egress_key`]）。
static SLOTS: OnceLock<Mutex<HashMap<String, Arc<Semaphore>>>> = OnceLock::new();

fn slot_table() -> &'static Mutex<HashMap<String, Arc<Semaphore>>> {
    SLOTS.get_or_init(|| Mutex::new(HashMap::new()))
}

/// 取该出口的一个在飞槽位。
///
/// 返回值三种：
///   - `Ok(Some(permit))` —— 拿到槽位，**permit 活着期间算一个在飞**；
///   - `Ok(None)` —— 内部状态坏了（锁中毒），**按不限流放行**。宁可冒一次
///     被标记的风险，也不要把整条转发永久堵死（与冷却表同一取向）；
///   - `Err(())` —— 等 [`SLOT_WAIT_MS`] 仍无空位，调用方应 503 快速失败。
///
/// permit 的归还时机由调用方决定（`forward` 把它绑在局部变量上，函数返回即
/// 归还 —— 流式是「响应头到手」，非流式是「聚合完成」）。
pub async fn acquire_slot(egress: &str) -> Result<Option<OwnedSemaphorePermit>, ()> {
    let semaphore = {
        let Ok(mut map) = slot_table().lock() else {
            return Ok(None);
        };
        // 顺手回收没人持有的条目：轮询档会不停换出口，键会一直变。
        // `OwnedSemaphorePermit` 内部持一份 Arc，所以「有在飞」时 strong_count ≥ 2。
        if map.len() > 64 {
            map.retain(|_, semaphore| Arc::strong_count(semaphore) > 1);
        }
        map.entry(egress.to_string())
            .or_insert_with(|| Arc::new(Semaphore::new(MAX_IN_FLIGHT_PER_EGRESS)))
            .clone()
    };
    let waited = tokio::time::timeout(
        std::time::Duration::from_millis(SLOT_WAIT_MS),
        semaphore.acquire_owned(),
    )
    .await;
    match waited {
        Ok(Ok(permit)) => Ok(Some(permit)),
        // 信号量被关闭：不可能发生（我们从不 close），按不限流放行
        Ok(Err(_)) => Ok(None),
        Err(_) => Err(()),
    }
}

/// 并发闸满时的快速失败文案。
pub fn slot_busy_message(egress: &str) -> String {
    format!(
        "OpenCode 出口「{}」的在飞请求已达上限（{} 路）：为了不被上游按 IP 标记，\
         网关会限制同时打这个出口的请求数。稍后重试即可；持续出现说明并发确实\
         超过了免费档能承受的量，可以考虑加一个出口（代理）。",
        egress_label(egress),
        MAX_IN_FLIGHT_PER_EGRESS,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lane_detection_covers_both_free_endpoints() {
        assert_eq!(
            lane_of("https://opencode.ai/inference/v1/"),
            Some(Lane::Inference)
        );
        assert_eq!(lane_of("https://opencode.ai/zen/v1"), Some(Lane::Zen));
        // Go 面不参与免费档判定
        assert_eq!(lane_of("https://opencode.ai/zen/go/v1"), None);
        assert_eq!(lane_of("https://example.com/v1"), None);
    }

    #[test]
    fn rejection_classification_prefers_specific_message() {
        // 前三种成因共用 FreeTierError 类型码，必须靠文案分开
        assert_eq!(
            classify("{\"error\":{\"type\":\"FreeTierError\",\"message\":\"OpenCode's free tier can only be used from within OpenCode\"}}"),
            Rejection::Gate
        );
        assert_eq!(
            classify("{\"type\":\"error\",\"error\":{\"type\":\"FreeTierError\",\"message\":\"This model is not available in your country\"}}"),
            Rejection::Region
        );
        assert_eq!(
            classify("{\"error\":{\"type\":\"FreeUsageLimitError\",\"message\":\"Free usage limit reached\"}}"),
            Rejection::Quota
        );
        // 模型不在免费档范围：与指纹/出口都无关，且**不该**被地区那条抢走
        assert_eq!(
            classify("{\"error\":{\"type\":\"FreeTierError\",\"message\":\"model claude-sonnet-5 is not available on the free tier\"}}"),
            Rejection::Model
        );
        assert_eq!(
            classify("{\"error\":{\"message\":\"Unknown model: gpt-5-pro\"}}"),
            Rejection::Model
        );
        // 只有类型码时按门禁兜底（最可修的一条）
        assert_eq!(classify("FreeTierError"), Rejection::Gate);
    }

    #[test]
    fn model_rejection_carries_no_cooldown() {
        // 一个错模型名不该把整家锁十分钟，也不该钉住出口
        assert_eq!(Rejection::Model.cooldown_ms(), None);
        assert!(!Rejection::Model.is_egress_scoped());
        assert_eq!(Rejection::Gate.cooldown_ms(), Some(GATE_COOLDOWN_MS));
        assert_eq!(Rejection::Quota.cooldown_ms(), Some(QUOTA_COOLDOWN_MS));
    }

    #[test]
    fn egress_key_is_address_not_label() {
        let proxy = ResolvedProxy {
            source: "pool".to_string(),
            protocol: "http".to_string(),
            host: "10.0.0.1".to_string(),
            port: Some(1080),
            username: String::new(),
            password: String::new(),
            label: "同一个机场".to_string(),
        };
        assert_eq!(egress_key(Some(&proxy)), "10.0.0.1:1080");
        // 直连是一个独立的「出口」
        assert_eq!(egress_key(None), "");
        assert_eq!(egress_label(""), "直连");
    }

    #[tokio::test]
    async fn egress_slots_are_bounded_and_isolated() {
        let a = "test-slot-a:1";
        let b = "test-slot-b:1";
        let mut held = Vec::new();
        for _ in 0..MAX_IN_FLIGHT_PER_EGRESS {
            let permit = match acquire_slot(a).await {
                Ok(permit) => permit,
                Err(()) => None,
            };
            held.push(permit);
        }
        assert!(
            held.iter().all(Option::is_some),
            "槽位充足时应当都拿到许可（而不是被放行或超时）"
        );
        // 第 N+1 个必须等：用外层短超时验证，别真等 SLOT_WAIT_MS
        let blocked = tokio::time::timeout(
            std::time::Duration::from_millis(50),
            acquire_slot(a),
        )
        .await;
        assert!(blocked.is_err(), "超出上限时必须阻塞，不能无限放行");
        // 另一个出口不受影响（并发闸按出口计，不按 provider 计）
        assert!(matches!(acquire_slot(b).await, Ok(Some(_))));
        // 归还一个即可再拿
        held.pop();
        assert!(matches!(acquire_slot(a).await, Ok(Some(_))));
    }

    #[test]
    fn cooldown_is_isolated_per_lane_and_scope() {
        let provider = "test-opencode-gate-lane";
        assert!(remaining_ms(provider, Lane::Zen, Some("1.1.1.1:8080")).is_none());
        // provider 级冷却：两条出口都被挡住，且状态码按写入的来
        mark(provider, Lane::Zen, None, 60_000, 403);
        assert!(remaining_ms(provider, Lane::Zen, Some("1.1.1.1:8080")).is_some());
        assert_eq!(
            remaining_ms(provider, Lane::Zen, Some("1.1.1.1:8080")).map(|(_, code)| code),
            Some(403)
        );
        // 按通道隔离：另一条通道不受影响
        assert!(remaining_ms(provider, Lane::Inference, Some("1.1.1.1:8080")).is_none());
        clear(provider, Lane::Zen);
        assert!(remaining_ms(provider, Lane::Zen, Some("1.1.1.1:8080")).is_none());
    }

    #[test]
    fn quota_cooldown_does_not_block_other_egress() {
        let provider = "test-opencode-gate-egress";
        // 额度是出口属性：钉住 A 出口后，轮询到 B 出口必须还能打
        mark(provider, Lane::Inference, Some("10.0.0.1:1080"), 60_000, 429);
        assert_eq!(
            remaining_ms(provider, Lane::Inference, Some("10.0.0.1:1080")).map(|(_, code)| code),
            Some(429)
        );
        assert!(remaining_ms(provider, Lane::Inference, Some("10.0.0.2:1080")).is_none());
        // 直连是独立的一个「出口」
        assert!(remaining_ms(provider, Lane::Inference, None).is_none());
        clear(provider, Lane::Inference);
        assert!(remaining_ms(provider, Lane::Inference, Some("10.0.0.1:1080")).is_none());
    }
}
