//! 发送结果类型（方案 v3 §六 D8）。
//!
//! M1 只落类型契约；HTTP 实现随 M2（mock webhook 集成测试）落地。
//! 契约要点：APP 与 CLI 都以 `feishu_code == 0` 且 HTTP 2xx 判定成功，
//! 修复现状「4xx/5xx 也返回 Ok 字符串」。

use serde::Serialize;

/// 错误类别（`--json` 的 `error.kind`；退出码映射见方案 §六）
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SendErrorKind {
    /// 用法/参数错误（退出码 1）
    Usage,
    /// 配置缺失/不可读（退出码 1）
    Config,
    /// 网络错误（退出码 2）
    Network,
    /// 超时（退出码 2）
    Timeout,
    /// HTTP 非 2xx（退出码 2）
    Http,
    /// 飞书业务错误（HTTP 200 但 code != 0，退出码 2）
    Feishu,
}

/// 类型化发送结果：core 唯一判定入口，APP 与 CLI 均基于此判断成功
#[derive(Debug, Clone, Serialize)]
pub struct SendResult {
    pub ok: bool,
    pub http_status: Option<u16>,
    /// 飞书响应 code（HTTP 200 时才有意义；0 = 成功）
    pub feishu_code: Option<i64>,
    pub feishu_msg: Option<String>,
    /// 响应 body 摘要（截断 512 字节，脱敏后）
    pub body_summary: Option<String>,
}
