//! 发送与图片上传（方案 v3 §三/§六，D7/D8）。
//!
//! 契约要点：
//! - core 返回类型化 `SendResult`，APP 与 CLI 都以 `feishu_code == 0` 且 HTTP 2xx 判定成功
//!   （修复现状「4xx/5xx 也返回 Ok 字符串」，review P1-3）
//! - history 只由 [`record_send`] 单一入口写入（D7）；重试由 Agent 显式发起，无自动重试
//! - 历史条目与 APP 前端 `rec` 形状兼容：`{time, kind, dir, summary, ok, status, payload, [text, bot]}`
//!   其中 `status` 沿用 APP 的展示文案（如 "200 OK" / "19021 xxx"）；三态判定写入
//!   `state` 字段：`sent` / `failed` / `unknown`（超时=消息可能已送达，不可假去重）

use crate::config::{check_url_policy, modify, Config};
use crate::message::hmac_sign;
use serde::Serialize;
use serde_json::{json, Value};
use std::path::Path;
use std::time::Duration;

pub const SEND_TIMEOUT: Duration = Duration::from_secs(15);
pub const UPLOAD_TIMEOUT: Duration = Duration::from_secs(60);
const BODY_SUMMARY_MAX: usize = 512;

/// 错误类别（`--json` 的 `error.kind`；退出码映射见方案 §六：usage/config→1，其余→2）
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SendErrorKind {
    /// 用法/参数错误（退出码 1）
    Usage,
    /// 配置缺失/不可读（退出码 1）
    Config,
    /// 网络错误（退出码 2）
    Network,
    /// 超时（退出码 2；消息可能已送达，历史记 unknown）
    Timeout,
    /// HTTP 非 2xx（退出码 2）
    Http,
    /// 飞书业务错误（HTTP 200 但 code != 0，退出码 2）
    Feishu,
}

/// 类型化发送结果
#[derive(Debug, Clone, Serialize)]
pub struct SendResult {
    pub ok: bool,
    pub http_status: Option<u16>,
    /// 飞书响应 code（0 = 成功；老版响应字段为 StatusCode）
    pub feishu_code: Option<i64>,
    pub feishu_msg: Option<String>,
    /// 响应 body 摘要（截断 512 字节）
    pub body_summary: Option<String>,
}

/// 发送失败（网络/超时层），与 HTTP 层结果区分
#[derive(Debug, Clone)]
pub struct SendFailure {
    pub kind: SendErrorKind,
    pub message: String,
}

impl SendFailure {
    fn new(kind: SendErrorKind, message: impl Into<String>) -> Self {
        SendFailure {
            kind,
            message: message.into(),
        }
    }
}

/// 一次发送的完整产物：HTTP 结果 + 待写入历史的条目
#[derive(Debug, Clone)]
pub struct RecordedSend {
    /// Ok = HTTP 层完成（ok 字段区分业务成败）；Err = 网络/超时层失败
    pub result: Result<SendResult, SendFailure>,
    /// 与 APP 兼容的历史条目（payload 为**签名前**payload，与 APP 行为一致）
    pub record: Value,
    /// 三态：sent / failed / unknown
    pub state: &'static str,
    /// 与 APP status 文案一致的展示串
    pub status_line: String,
}

fn http_client(timeout: Duration) -> Result<reqwest::blocking::Client, SendFailure> {
    reqwest::blocking::Client::builder()
        .timeout(timeout)
        .build()
        .map_err(|e| SendFailure::new(SendErrorKind::Network, format!("创建 HTTP 客户端失败: {e}")))
}

fn truncate_bytes(s: &str, max: usize) -> String {
    if s.len() <= max {
        return s.to_string();
    }
    let mut end = max;
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…(截断)", &s[..end])
}

fn feishu_code_of(body: &Value) -> Option<i64> {
    body.get("code")
        .or_else(|| body.get("StatusCode"))
        .and_then(|v| v.as_i64())
}

fn feishu_msg_of(body: &Value) -> Option<String> {
    body.get("msg")
        .or_else(|| body.get("StatusMessage"))
        .and_then(|v| v.as_str())
        .map(String::from)
}

/// APP status 文案兼容：`HTTP {n}` / `200 OK` / `{code} {msg}`
fn status_line_for(http_status: u16, body: &Value) -> String {
    match feishu_code_of(body) {
        Some(0) => "200 OK".to_string(),
        Some(code) => format!("{code} {}", feishu_msg_of(body).unwrap_or_default()),
        None => format!("HTTP {http_status}"),
    }
}

/// 按飞书自定义机器人规则给 payload 追加 timestamp+sign（secret 为空则不签，与 APP 一致）
pub fn build_signed_payload(bot_secret: &str, payload: &Value, timestamp_secs: u64) -> Value {
    if bot_secret.is_empty() {
        return payload.clone();
    }
    let ts = timestamp_secs.to_string();
    let sign = hmac_sign(bot_secret, &ts);
    let mut signed = payload.clone();
    let obj = signed.as_object_mut().expect("payload 必须是对象");
    obj.insert("timestamp".into(), json!(ts));
    obj.insert("sign".into(), json!(sign));
    signed
}

/// 发送 webhook（仅 HTTP 层，不写历史）。历史写入走 [`record_send`]。
pub fn send_webhook(url: &str, signed_payload: &Value) -> Result<SendResult, SendFailure> {
    let client = http_client(SEND_TIMEOUT)?;
    let resp = client
        .post(url)
        .json(signed_payload)
        .send()
        .map_err(|e| {
            if e.is_timeout() {
                SendFailure::new(
                    SendErrorKind::Timeout,
                    format!("请求超时（{}s），消息可能已送达，请勿盲目重试", SEND_TIMEOUT.as_secs()),
                )
            } else {
                SendFailure::new(SendErrorKind::Network, format!("请求失败: {e}"))
            }
        })?;
    let status = resp.status();
    let body_text = resp
        .text()
        .map_err(|e| SendFailure::new(SendErrorKind::Network, format!("读取响应失败: {e}")))?;
    let body: Value = serde_json::from_str(&body_text).unwrap_or(Value::Null);
    let feishu_code = feishu_code_of(&body);
    // 与 APP 判定一致：2xx 且（无 code 字段或 code == 0）为成功
    let ok = status.is_success() && feishu_code.map(|c| c == 0).unwrap_or(true);
    Ok(SendResult {
        ok,
        http_status: Some(status.as_u16()),
        feishu_code,
        feishu_msg: feishu_msg_of(&body),
        body_summary: Some(truncate_bytes(&body_text, BODY_SUMMARY_MAX)),
    })
}

/// **history 单一写入口**（D7）：发送完成后锁内 read-modify-write 追加历史并落盘。
/// `payload` 必须是签名前的 payload（APP 同款行为，避免把签名材料写进配置）。
pub fn record_send(
    config_path: &Path,
    rec: Value,
) -> Result<(), String> {
    modify(config_path, |cfg| {
        cfg.push_history(rec);
        Ok(())
    })
}

/// 一次发送尝试：bot 已解析、payload 已签名、HTTP 已执行。
/// `result` 的 Err 是网络/超时层失败（仍属「已尝试」，调用方需记录 failed/unknown 历史）；
/// 本函数自身的 Err 是 usage/config 级失败（bot 不存在 / URL 策略），发生在发送之前。
pub struct SendAttempt {
    pub bot: crate::config::BotView,
    pub result: Result<SendResult, SendFailure>,
}

/// 解析目标机器人 + URL 策略 + 组装签名 payload + 发送（HTTP 层）。
/// dispatch_send（含历史记录构造）与 APP 的 resend 都复用此入口。
pub fn send_prebuilt(
    cfg: &Config,
    bot_key: Option<&str>,
    payload: &Value,
    allow_insecure: bool,
    now_secs: u64,
) -> Result<SendAttempt, SendFailure> {
    let bot = match bot_key {
        Some(key) => cfg.find_bot(key).ok_or_else(|| {
            SendFailure::new(SendErrorKind::Usage, format!("未找到机器人: {key}"))
        })?,
        None => cfg.default_bot().ok_or_else(|| {
            SendFailure::new(
                SendErrorKind::Config,
                "未配置机器人，请先 qingniao bot add 或在 APP 中添加",
            )
        })?,
    };
    check_url_policy(&bot.url, allow_insecure)
        .map_err(|m| SendFailure::new(SendErrorKind::Usage, m))?;
    let signed = build_signed_payload(&bot.secret, payload, now_secs);
    let result = send_webhook(&bot.url, &signed);
    Ok(SendAttempt { bot, result })
}

/// SendResult → 与 APP status 文案兼容的展示串（`200 OK` / `{code} {msg}` / `HTTP {n}`）
pub fn status_line_of(r: &SendResult) -> String {
    let body = r
        .body_summary
        .as_deref()
        .and_then(|s| serde_json::from_str::<Value>(s).ok())
        .unwrap_or(Value::Null);
    status_line_for(r.http_status.unwrap_or(0), &body)
}

/// 端到端发送：组装签名 payload（调用方先用 `crate::message::build_payload` 组装）→
/// HTTP → 产出 [`RecordedSend`]。写历史由调用方拿到 record 后调 [`record_send`]，
/// 以便 CLI 先处理 dry-run（不发送也不写历史）。
pub fn dispatch_send(
    cfg: &Config,
    bot_key: Option<&str>,
    payload: &Value,
    allow_insecure_url: bool,
    now_secs: u64,
) -> Result<RecordedSend, SendFailure> {
    let attempt = send_prebuilt(cfg, bot_key, payload, allow_insecure_url, now_secs)?;
    let bot_name = attempt.bot.name.clone();
    let result = attempt.result;

    let msg_type = payload
        .get("msg_type")
        .and_then(|v| v.as_str())
        .unwrap_or("?")
        .to_string();
    let (state, status_line, ok_flag) = match &result {
        Ok(r) => {
            let line = status_line_of(r);
            let state = if r.ok { "sent" } else { "failed" };
            (state, line, r.ok)
        }
        Err(f) => {
            let state = if f.kind == SendErrorKind::Timeout { "unknown" } else { "failed" };
            (state, f.message.clone(), false)
        }
    };

    let record = json!({
        "time": iso8601_now(now_secs),
        "kind": msg_type,
        "dir": "out",
        "summary": payload_summary(payload),
        "ok": ok_flag,
        "status": status_line,
        "state": state,
        "payload": payload,
        "bot": bot_name,
    });

    Ok(RecordedSend {
        result,
        record,
        state,
        status_line,
    })
}

fn payload_summary(payload: &Value) -> String {
    // 与 APP makePreview 同规则：首行前 60 字符；image 类型由调用方覆盖为文件名列表
    let msg_type = payload.get("msg_type").and_then(|v| v.as_str()).unwrap_or("");
    let first_line = match payload.pointer("/content/text") {
        Some(Value::String(s)) => s.lines().next().unwrap_or("").to_string(),
        _ => {
            // post / interactive：取 title 或 lark_md 首行做摘要
            payload
                .pointer("/content/post/zh_cn/title")
                .and_then(|v| v.as_str())
                .map(String::from)
                .or_else(|| {
                    payload.pointer("/card/header/title/content").and_then(|v| v.as_str()).map(String::from)
                })
                .unwrap_or_default()
        }
    };
    let preview: String = first_line.chars().take(60).collect();
    if msg_type == "image" {
        return "图片消息".into();
    }
    if msg_type == "interactive" {
        return "交互卡片消息".into();
    }
    if preview.is_empty() {
        "(空)".into()
    } else {
        preview
    }
}

pub fn iso8601_now(now_secs: u64) -> String {
    // 与 APP nowIso() 同为 UTC ISO8601；core 无 chrono 依赖，手写格式化
    let days_since_epoch = now_secs / 86400;
    let secs_of_day = now_secs % 86400;
    let (y, m, d) = civil_from_days(days_since_epoch as i64);
    format!(
        "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}.000Z",
        y,
        m,
        d,
        secs_of_day / 3600,
        (secs_of_day % 3600) / 60,
        secs_of_day % 60
    )
}

/// Howard Hinnant 的 days_from_civil 逆变换（公历）
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719468;
    let era = if z >= 0 { z } else { z - 146096 } / 146097;
    let doe = (z - era * 146097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

/// 上传图片换 image_key（端点与 APP upload_image 一致：token 换取 → multipart 上传）
pub fn upload_image(
    app_id: &str,
    app_secret: &str,
    image_bytes: Vec<u8>,
    filename: &str,
) -> Result<String, SendFailure> {
    if app_id.is_empty() || app_secret.is_empty() {
        return Err(SendFailure::new(
            SendErrorKind::Config,
            "未配置飞书应用凭证（App ID / App Secret），无法上传图片",
        ));
    }
    let client = http_client(UPLOAD_TIMEOUT)?;

    let token_resp = client
        .post("https://open.feishu.cn/open-apis/auth/v3/tenant_access_token/internal")
        .json(&json!({ "app_id": app_id, "app_secret": app_secret }))
        .send()
        .map_err(|e| {
            if e.is_timeout() {
                SendFailure::new(SendErrorKind::Timeout, format!("获取 token 超时: {e}"))
            } else {
                SendFailure::new(SendErrorKind::Network, format!("获取 token 请求失败: {e}"))
            }
        })?;
    if !token_resp.status().is_success() {
        return Err(SendFailure::new(
            SendErrorKind::Http,
            format!("获取 token HTTP {}", token_resp.status().as_u16()),
        ));
    }
    let token_json: Value = token_resp
        .json()
        .map_err(|e| SendFailure::new(SendErrorKind::Http, format!("token 响应解析失败: {e}")))?;
    if feishu_code_of(&token_json).unwrap_or(-1) != 0 {
        return Err(SendFailure::new(
            SendErrorKind::Feishu,
            format!(
                "获取 token 失败: {} {}",
                feishu_code_of(&token_json).unwrap_or(-1),
                feishu_msg_of(&token_json).unwrap_or_default()
            ),
        ));
    }
    let token = token_json["tenant_access_token"]
        .as_str()
        .ok_or_else(|| SendFailure::new(SendErrorKind::Http, "token 响应中缺少 tenant_access_token"))?
        .to_string();

    let part = reqwest::blocking::multipart::Part::bytes(image_bytes).file_name(filename.to_string());
    let form = reqwest::blocking::multipart::Form::new()
        .text("image_type", "message")
        .part("image", part);
    let upload_resp = client
        .post("https://open.feishu.cn/open-apis/im/v1/images")
        .bearer_auth(token)
        .multipart(form)
        .send()
        .map_err(|e| {
            if e.is_timeout() {
                SendFailure::new(SendErrorKind::Timeout, format!("上传超时: {e}"))
            } else {
                SendFailure::new(SendErrorKind::Network, format!("上传图片请求失败: {e}"))
            }
        })?;
    let status = upload_resp.status();
    let upload_json: Value = upload_resp
        .json()
        .map_err(|e| SendFailure::new(SendErrorKind::Http, format!("上传响应解析失败: {e}")))?;
    if !status.is_success() {
        return Err(SendFailure::new(
            SendErrorKind::Http,
            format!("上传 HTTP {}", status.as_u16()),
        ));
    }
    if feishu_code_of(&upload_json).unwrap_or(-1) != 0 {
        return Err(SendFailure::new(
            SendErrorKind::Feishu,
            format!(
                "上传图片失败: {} {}",
                feishu_code_of(&upload_json).unwrap_or(-1),
                feishu_msg_of(&upload_json).unwrap_or_default()
            ),
        ));
    }
    upload_json["data"]["image_key"]
        .as_str()
        .map(String::from)
        .ok_or_else(|| {
            SendFailure::new(SendErrorKind::Http, "上传响应中缺少 image_key")
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn signed_payload_appends_fields() {
        let p = json!({"msg_type":"text","content":{"text":"hi"}});
        let signed = build_signed_payload("sec", &p, 1700000000);
        assert_eq!(signed["timestamp"], "1700000000");
        assert!(signed["sign"].is_string());
        // 原 payload 不被修改
        assert!(p.get("sign").is_none());
    }

    #[test]
    fn empty_secret_no_sign() {
        let p = json!({"msg_type":"text","content":{"text":"hi"}});
        let signed = build_signed_payload("", &p, 1700000000);
        assert!(signed.get("timestamp").is_none());
    }

    #[test]
    fn status_line_compat() {
        assert_eq!(status_line_for(200, &json!({"code":0,"msg":"success"})), "200 OK");
        assert_eq!(status_line_for(200, &json!({"code":19021,"msg":"sign match error"})), "19021 sign match error");
        assert_eq!(status_line_for(500, &Value::Null), "HTTP 500");
    }

    #[test]
    fn iso_format() {
        assert_eq!(iso8601_now(0), "1970-01-01T00:00:00.000Z");
        assert_eq!(iso8601_now(1700000000), "2023-11-14T22:13:20.000Z");
    }
}
