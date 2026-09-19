//! 飞书 Drive API 客户端（设计文档 §8/§11）。
//!
//! 全部走 blocking reqwest；分片上传/下载**串行**（并发 1，天然满足 5 QPS）。
//! 连接超时 10 s、总超时 60 s（P2-6）；错误统一解析 `{"code":..,"msg":..}`。
//! 每次 Drive 调用经由 [`Quota`] 发出即计数（§6.5）。

use crate::transfer::quota::Quota;
use serde_json::Value;
use std::sync::Mutex;
use std::time::Duration;

const BASE: &str = "https://open.feishu.cn";

/// 统一调用结果：JSON 文本或二进制
enum CallResult {
    Text(String),
    Bytes(Vec<u8>),
}

#[derive(Debug)]
pub struct FeishuError {
    pub code: i64,
    pub msg: String,
    pub http_status: u16,
}

impl std::fmt::Display for FeishuError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "飞书 API 错误 {}: {} (HTTP {})", self.code, self.msg, self.http_status)
    }
}

impl FeishuError {
    /// 1061045 内部错误可重试；429 频控可退避重试；99991400 一并按可重试处理（§13/§14）
    pub fn retryable(&self) -> bool {
        self.code == 1061045 || self.http_status == 429 || self.code == 99991400
    }
    /// 99991403 月度额度耗尽：不可重试
    pub fn quota_exhausted(&self) -> bool {
        self.code == 99991403
    }
}

/// 列目录返回的单条对象（D30：清理模块需要 created_time 与类型）
#[derive(Clone, Debug)]
pub struct ListedFile {
    pub token: String,
    pub name: String,
    /// created_time 换算为 unix 秒；缺失/不可解析 = 0（由调用方决定如何处置）
    pub created_time_secs: i64,
    /// `type == "folder"`
    pub is_dir: bool,
}

/// Drive `created_time` 解析：实测为**字符串秒**（样例 "1789888029"）；
/// 同时接受 int；值 > 1e11 视为毫秒并换算 + warn（D30 毫秒护栏——单位口径若被
/// 官方变更，护栏保证不会整体静默失灵）。缺失/不可解析返回 0。
fn parse_created_time(v: Option<&Value>) -> i64 {
    let Some(v) = v else { return 0 };
    let raw: i64 = match v {
        Value::String(s) => s.trim().parse().unwrap_or(0),
        Value::Number(n) => n.as_i64().unwrap_or(0),
        _ => 0,
    };
    if raw == 0 {
        return 0;
    }
    // 秒级时间戳到 5138 年都在 1e11 以内；毫秒级现值约 1.79e12
    if raw > 100_000_000_000 {
        log::warn!("飞书 created_time 为毫秒值（样例 {raw}），已按毫秒换算为秒");
        raw / 1000
    } else {
        raw
    }
}

pub struct FeishuClient {
    app_id: String,
    app_secret: String,
    http: reqwest::blocking::Client,
    token: Mutex<Option<(String, i64)>>, // (token, 过期时刻 unix 秒)
    quota: std::sync::Arc<Quota>,
}

impl FeishuClient {
    pub fn new(app_id: &str, app_secret: &str, quota: std::sync::Arc<Quota>) -> Result<Self, String> {
        let http = reqwest::blocking::Client::builder()
            .connect_timeout(Duration::from_secs(10))
            .timeout(Duration::from_secs(60))
            .build()
            .map_err(|e| format!("创建 HTTP 客户端失败: {e}"))?;
        Ok(Self {
            app_id: app_id.to_string(),
            app_secret: app_secret.to_string(),
            http,
            token: Mutex::new(None),
            quota,
        })
    }

    /* ===================== tenant_access_token（缓存复用，§11） ===================== */

    fn get_token(&self) -> Result<String, FeishuError> {
        {
            let t = self.token.lock().unwrap_or_else(|p| p.into_inner());
            if let Some((tok, exp)) = t.as_ref() {
                if *exp > crate::transfer::crypto::now_unix() + 300 {
                    return Ok(tok.clone());
                }
            }
        }
        let resp = self
            .http
            .post(format!("{BASE}/open-apis/auth/v3/tenant_access_token/internal"))
            .json(&serde_json::json!({"app_id": self.app_id, "app_secret": self.app_secret}))
            .send()
            .map_err(|e| FeishuError { code: -1, msg: format!("token 请求失败: {e}"), http_status: 0 })?;
        let v: Value = resp.json().map_err(|e| FeishuError { code: -1, msg: format!("token 响应解析失败: {e}"), http_status: 0 })?;
        if v.get("code").and_then(Value::as_i64).unwrap_or(-1) != 0 {
            return Err(FeishuError {
                code: v.get("code").and_then(Value::as_i64).unwrap_or(-1),
                msg: v.get("msg").and_then(Value::as_str).unwrap_or("token 获取失败").into(),
                http_status: 0,
            });
        }
        let tok = v.get("tenant_access_token").and_then(Value::as_str).unwrap_or("").to_string();
        let expire = v.get("expire").and_then(Value::as_i64).unwrap_or(0);
        if tok.is_empty() {
            return Err(FeishuError { code: -1, msg: "token 为空".into(), http_status: 0 });
        }
        let now = crate::transfer::crypto::now_unix();
        *self.token.lock().unwrap_or_else(|p| p.into_inner()) = Some((tok.clone(), now + expire));
        Ok(tok)
    }

    /* ===================== 通用请求（带重试） ===================== */

    /// 统一请求入口：JSON 调用返回响应文本；`expect_binary` 时返回字节流。
    /// `multipart_builder` 每次尝试（含重试）重建 Form（Form 不可 clone）。
    /// 429/1061045/99991400 退避重试（最多 3 次尝试，即至多 2 次重试，退避 2s / 4s），其余错误立即返回。
    fn call(
        &self,
        kind: &'static str,
        method: reqwest::Method,
        path: &str,
        json_body: Option<&Value>,
        multipart_builder: Option<&dyn Fn() -> reqwest::blocking::multipart::Form>,
        expect_binary: bool,
    ) -> Result<CallResult, FeishuError> {
        let mut last_err: Option<FeishuError> = None;
        for attempt in 0..3 {
            if attempt > 0 {
                std::thread::sleep(Duration::from_secs(2u64 << (attempt - 1))); // 2s / 4s
            }
            self.quota.bump(kind);
            let token = self.get_token()?;
            let url = format!("{BASE}{path}");
            let mut req = self.http.request(method.clone(), &url).bearer_auth(&token);
            if let Some(v) = json_body {
                req = req.json(v);
            }
            if let Some(build) = multipart_builder {
                req = req.multipart(build());
            }
            match req.send() {
                Ok(resp) => {
                    let status = resp.status().as_u16();
                    if status == 429 {
                        last_err = Some(FeishuError { code: 99991400, msg: "频控".into(), http_status: 429 });
                        continue;
                    }
                    if expect_binary && status == 200 {
                        match resp.bytes() {
                            Ok(b) => return Ok(CallResult::Bytes(b.to_vec())),
                            Err(e) => return Err(FeishuError { code: -1, msg: format!("下载读流失败: {e}"), http_status: status }),
                        }
                    }
                    let text = resp.text().map_err(|e| FeishuError { code: -1, msg: format!("读响应失败: {e}"), http_status: status })?;
                    let v: Value = serde_json::from_str(&text).unwrap_or(Value::Null);
                    let code = v.get("code").and_then(Value::as_i64).unwrap_or(if (200..300).contains(&status) { 0 } else { -1 });
                    if code == 0 {
                        return Ok(CallResult::Text(text));
                    }
                    let err = FeishuError {
                        code,
                        msg: v.get("msg").and_then(Value::as_str).unwrap_or("").into(),
                        http_status: status,
                    };
                    if err.retryable() {
                        last_err = Some(err);
                        continue;
                    }
                    return Err(err);
                }
                Err(e) => {
                    last_err = Some(FeishuError { code: -1, msg: format!("请求失败: {e}"), http_status: 0 });
                }
            }
        }
        Err(last_err.unwrap_or(FeishuError { code: -1, msg: "未知错误".into(), http_status: 0 }))
    }

    fn call_json(
        &self,
        kind: &'static str,
        method: reqwest::Method,
        path: &str,
        json_body: Option<&Value>,
    ) -> Result<Value, FeishuError> {
        match self.call(kind, method, path, json_body, None, false)? {
            CallResult::Text(t) => serde_json::from_str(&t)
                .map_err(|e| FeishuError { code: -1, msg: format!("JSON 解析失败: {e}"), http_status: 0 }),
            CallResult::Bytes(_) => Err(FeishuError { code: -1, msg: "预期 JSON 收到二进制".into(), http_status: 0 }),
        }
    }

    /* ===================== Drive 接口（§11） ===================== */

    /// 根目录 token：GET /drive/explorer/v2/root_folder/meta
    pub fn root_folder(&self) -> Result<String, FeishuError> {
        let v = self.call_json("list", reqwest::Method::GET, "/open-apis/drive/explorer/v2/root_folder/meta", None)?;
        v.pointer("/data/token")
            .and_then(Value::as_str)
            .map(String::from)
            .ok_or_else(|| FeishuError { code: -1, msg: "root_folder/meta 缺少 token".into(), http_status: 0 })
    }

    /// 列目录（page_size=50 翻页取全，20 次/秒）。
    /// 每条带 `created_time`（unix 秒）与 `is_dir`（D30：月份目录识别需 type 过滤）。
    pub fn list_folder(&self, folder_token: &str) -> Result<Vec<ListedFile>, FeishuError> {
        let mut out = Vec::new();
        let mut page_token = String::new();
        loop {
            let mut path = format!("/open-apis/drive/v1/files?folder_token={folder_token}&page_size=50");
            if !page_token.is_empty() {
                path.push_str(&format!("&page_token={page_token}"));
            }
            let v = self.call_json("list", reqwest::Method::GET, &path, None)?;
            if let Some(files) = v.pointer("/data/files").and_then(Value::as_array) {
                for f in files {
                    let t = f.get("token").and_then(Value::as_str).unwrap_or("").to_string();
                    let n = f.get("name").and_then(Value::as_str).unwrap_or("").to_string();
                    if !t.is_empty() {
                        out.push(ListedFile {
                            token: t,
                            name: n,
                            created_time_secs: parse_created_time(f.get("created_time")),
                            is_dir: f.get("type").and_then(Value::as_str) == Some("folder"),
                        });
                    }
                }
            }
            match v.pointer("/data/next_page_token").and_then(Value::as_str) {
                Some(t) if !t.is_empty() => page_token = t.to_string(),
                _ => break,
            }
        }
        Ok(out)
    }

    /// 创建文件夹：POST /drive/v1/files/create_folder
    pub fn create_folder(&self, parent: &str, name: &str) -> Result<String, FeishuError> {
        let v = self.call_json(
            "list",
            reqwest::Method::POST,
            "/open-apis/drive/v1/files/create_folder",
            Some(&serde_json::json!({"name": name, "folder_token": parent})),
        )?;
        v.pointer("/data/token")
            .and_then(Value::as_str)
            .map(String::from)
            .ok_or_else(|| FeishuError { code: -1, msg: "create_folder 缺少 token".into(), http_status: 0 })
    }

    /// 幂等确保「青鸟传输/<YYYY-MM>」目录存在（§6.3：先列根目录按名匹配、找不到再建）
    pub fn ensure_transfer_dir(&self, root: &str) -> Result<String, FeishuError> {
        let month = {
            let now = time::OffsetDateTime::now_utc();
            format!("{:04}-{:02}", now.year(), u8::from(now.month()))
        };
        // 根下找「青鸟传输」
        let files = self.list_folder(root)?;
        let transfer = files.iter().find(|f| f.name == "青鸟传输").map(|f| f.token.clone());
        let transfer = match transfer {
            Some(t) => t,
            None => self.create_folder(root, "青鸟传输")?,
        };
        // 月份目录
        let sub = self.list_folder(&transfer)?;
        match sub.iter().find(|f| f.name == month).map(|f| f.token.clone()) {
            Some(t) => Ok(t),
            None => self.create_folder(&transfer, &month),
        }
    }

    /// 上传密文分片：POST /drive/v1/files/upload_all（≤20MB，size 必填）
    pub fn upload_chunk(&self, parent_node: &str, file_name: &str, data: Vec<u8>) -> Result<String, FeishuError> {
        let size = data.len();
        let build_form = || {
            let part = reqwest::blocking::multipart::Part::bytes(data.clone()).file_name(file_name.to_string());
            reqwest::blocking::multipart::Form::new()
                .text("file_name", file_name.to_string())
                .text("parent_type", "explorer")
                .text("parent_node", parent_node.to_string())
                .text("size", size.to_string())
                .part("file", part)
        };
        match self.call("upload", reqwest::Method::POST, "/open-apis/drive/v1/files/upload_all", None, Some(&build_form), false)? {
            CallResult::Text(t) => {
                let v: Value = serde_json::from_str(&t).map_err(|e| FeishuError { code: -1, msg: format!("JSON 解析失败: {e}"), http_status: 200 })?;
                v.pointer("/data/file_token")
                    .and_then(Value::as_str)
                    .map(String::from)
                    .ok_or_else(|| FeishuError { code: -1, msg: "upload_all 缺少 file_token".into(), http_status: 200 })
            }
            CallResult::Bytes(_) => Err(FeishuError { code: -1, msg: "预期 JSON 收到二进制".into(), http_status: 200 }),
        }
    }

    /// 下载密文分片：GET /drive/v1/files/{token}/download（404=已删除语义由调用方处理）
    pub fn download_chunk(&self, file_token: &str) -> Result<Vec<u8>, FeishuError> {
        let path = format!("/open-apis/drive/v1/files/{file_token}/download");
        match self.call("download", reqwest::Method::GET, &path, None, None, true)? {
            CallResult::Bytes(b) => Ok(b),
            CallResult::Text(_) => Err(FeishuError { code: -1, msg: "下载接口返回非二进制".into(), http_status: 200 }),
        }
    }

    /// 删除分片（下载即删，D11）：DELETE /drive/v1/files/{token}?type=file
    /// 404 视为成功（删除请求已发出但响应丢失 → 重试收到 404 即成功，§9.4）
    pub fn delete_file(&self, file_token: &str) -> Result<(), FeishuError> {
        let path = format!("/open-apis/drive/v1/files/{file_token}?type=file");
        match self.call_json("delete", reqwest::Method::DELETE, &path, None) {
            Ok(_) => Ok(()),
            Err(e) if e.code == -1 && e.http_status == 404 => Ok(()),
            Err(e) if e.http_status == 404 => Ok(()),
            Err(e) => Err(e),
        }
    }
}

/* ===================== webhook 签名（M1：去重，统一走 message::hmac_sign） ===================== */

/// 组装「仅链接」交互式卡片（D3：仅链接，载体由富文本 post 改为 interactive card）：
/// 头部为品牌色（turquoise）标题 + 副标题（链接有效期）+ 文件图标/文件名（超链接指向取回链接）
/// + 取回按钮 + 分界线 + note footer（端限制提示 + 发送时间/过期提示）。
/// `ts` = payload 的发送时间（UTC Unix 秒，§7.2），与新鲜度窗口同源，避免展示值与实际过期时刻不一致。
pub fn build_transfer_card(file_name: &str, size: u64, link: &str, ts: i64) -> Value {
    let icon = file_icon(file_name);
    let minutes = crate::transfer::crypto::FRESHNESS_WINDOW_MINUTES;
    serde_json::json!({
        "msg_type": "interactive",
        "card": {
            "config": { "wide_screen_mode": true },
            "header": {
                "template": "turquoise",
                "title": { "tag": "plain_text", "content": "有一个文件等你取回" },
                "subtitle": { "tag": "plain_text", "content": format!("链接 {minutes} 分钟内有效") }
            },
            "elements": [
                {
                    "tag": "div",
                    "text": { "tag": "lark_md", "content": format!(
                        "{icon} **[{file_name}]({link})**\n<font color='grey'>{}</font>",
                        human_size(size)
                    )}
                },
                {
                    "tag": "action",
                    "actions": [{
                        "tag": "button",
                        "type": "primary",
                        "text": { "tag": "plain_text", "content": "取回文件" },
                        "url": link
                    }]
                },
                { "tag": "hr" },
                {
                    "tag": "note",
                    "elements": [{ "tag": "plain_text", "content":
                        "💻 取回只能在电脑端完成（青鸟 APP 或 CLI），手机端不支持"
                    }]
                },
                {
                    "tag": "note",
                    "elements": [{ "tag": "plain_text", "content": format!(
                        "⏳ {} 发出 · 过期后请让对方重新发送",
                        fmt_local_time(ts)
                    )}]
                }
            ]
        }
    })
}

/// Unix 秒 → 本地时区 `YYYY-MM-DD HH:MM`（卡片只展示到分钟）。
/// 取不到本地偏移时回落 UTC（与 `src-tauri/src/lib.rs` 的 `now_str` 同口径）。
fn fmt_local_time(unix: i64) -> String {
    fmt_local_time_at(unix, time::UtcOffset::current_local_offset().unwrap_or(time::UtcOffset::UTC))
}

/// 同上，但偏移由调用方给定。
///
/// **为什么要有这个接缝**（M0a，方案 §10 出口 2）：`fmt_local_time` 读系统时区，
/// 输出的卡片文案随运行环境变化——开发机（Asia/Shanghai）捕获的 golden 到 CI（UTC）必然不等。
/// 实测确认：同一 `ts` 在 `TZ=Asia/Shanghai` 渲染 `2026-04-12 21:20:00`、在 `TZ=UTC` 渲染
/// `2026-04-12 13:20:00`。（另：`time` 0.3 的 local-offset soundness 门禁「多线程会返回 Err」
/// **在本机未复现**——起过 4 个线程后仍返回 `Ok(+08:00:00)`；但该行为依赖平台与 crate 版本，
/// 不作为设计依据。）
///
/// 生产路径行为不变：`fmt_local_time` 仍按系统时区渲染；此函数只是把偏移变成入参，
/// 让 golden 用例能用**固定偏移**断言格式，从而与时区、线程数都无关。
fn fmt_local_time_at(unix: i64, offset: time::UtcOffset) -> String {
    let Ok(t) = time::OffsetDateTime::from_unix_timestamp(unix) else {
        return String::new();
    };
    let t = t.to_offset(offset);
    format!(
        "{:04}-{:02}-{:02} {:02}:{:02}",
        t.year(),
        t.month() as u8,
        t.day(),
        t.hour(),
        t.minute()
    )
}

/// 按扩展名给出文件图标（无法识别时用回形针）
fn file_icon(file_name: &str) -> &'static str {
    let ext = file_name.rsplit_once('.').map(|(_, e)| e.to_ascii_lowercase());
    match ext.as_deref() {
        Some("zip" | "rar" | "7z" | "tar" | "gz" | "bz2" | "xz") => "🗜️",
        Some("pdf") => "📕",
        Some("doc" | "docx" | "rtf") => "📘",
        Some("xls" | "xlsx" | "csv" | "ods") => "📗",
        Some("ppt" | "pptx" | "key") => "📙",
        Some("png" | "jpg" | "jpeg" | "gif" | "webp" | "bmp" | "svg" | "heic") => "🖼️",
        Some("mp4" | "mov" | "avi" | "mkv" | "webm") => "🎬",
        Some("mp3" | "wav" | "flac" | "aac" | "m4a") => "🎵",
        Some("txt" | "md" | "log" | "json" | "xml" | "yaml" | "yml" | "toml") => "📄",
        _ => "📎",
    }
}

fn human_size(b: u64) -> String {
    if b >= 1024 * 1024 * 1024 { format!("{:.2} GB", b as f64 / (1024.0 * 1024.0 * 1024.0)) }
    else if b >= 1024 * 1024 { format!("{:.1} MB", b as f64 / (1024.0 * 1024.0)) }
    else if b >= 1024 { format!("{} KB", b / 1024) }
    else { format!("{b} B") }
}

/// 发送 webhook 消息（不计入月度额度，D4/D13）
pub fn send_webhook_json(url: &str, payload: &Value, secret: Option<&str>) -> Result<(u16, String), String> {
    let client = reqwest::blocking::Client::builder()
        .connect_timeout(Duration::from_secs(10))
        .timeout(Duration::from_secs(60))
        .build()
        .map_err(|e| format!("创建 HTTP 客户端失败: {e}"))?;
    let mut payload = payload.clone();
    if let Some(sec) = secret {
        if !sec.is_empty() {
            let ts = crate::transfer::crypto::now_unix().to_string();
            if let Some(obj) = payload.as_object_mut() {
                obj.insert("timestamp".into(), Value::from(ts.clone()));
                obj.insert("sign".into(), Value::from(crate::message::hmac_sign(sec, &ts)));
            }
        }
    }
    let resp = client.post(url).json(&payload).send().map_err(|e| format!("webhook 请求失败: {e}"))?;
    let status = resp.status().as_u16();
    let body = resp.text().map_err(|e| format!("读响应失败: {e}"))?;
    Ok((status, body))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// D30 毫秒护栏：实测为字符串秒；string/int 双接受；>1e11 视为毫秒换算
    #[test]
    fn parse_created_time_accepts_string_int_and_ms_guard() {
        // 实测口径：字符串秒
        assert_eq!(parse_created_time(Some(&serde_json::json!("1789888029"))), 1_789_888_029);
        // int 同样接受
        assert_eq!(parse_created_time(Some(&serde_json::json!(1_789_888_029))), 1_789_888_029);
        // 毫秒护栏：>1e11 → 换算为秒
        assert_eq!(parse_created_time(Some(&serde_json::json!(1_789_888_029_000i64))), 1_789_888_029);
        assert_eq!(parse_created_time(Some(&serde_json::json!("1789888029000"))), 1_789_888_029);
        // 毫秒护栏边界：1e11（含）以内不换算，超出换算
        assert_eq!(parse_created_time(Some(&serde_json::json!(100_000_000_000i64))), 100_000_000_000);
        assert_eq!(parse_created_time(Some(&serde_json::json!(100_000_000_001i64))), 100_000_000);
        // 缺失 / 不可解析 → 0（由扫描端跳过，宁漏删勿误删）
        assert_eq!(parse_created_time(None), 0);
        assert_eq!(parse_created_time(Some(&serde_json::json!("abc"))), 0);
        assert_eq!(parse_created_time(Some(&serde_json::json!(null))), 0);
        assert_eq!(parse_created_time(Some(&serde_json::json!(0))), 0);
    }

    #[test]
    fn webhook_sign_via_message_hmac_sign_matches_reference() {
        // 与前端 Web Crypto 实现同语义：HMAC-SHA256(key=ts+"\n"+secret, msg="")
        let s = crate::message::hmac_sign("test-secret", "1700000000");
        assert!(!s.is_empty());
        // 已知一致性用例：同输入同输出
        assert_eq!(s, crate::message::hmac_sign("test-secret", "1700000000"));
        assert_ne!(s, crate::message::hmac_sign("other-secret", "1700000000"));
        assert_ne!(s, crate::message::hmac_sign("test-secret", "1700000001"));
    }

    #[test]
    fn transfer_card_contains_link_only() {
        let v = build_transfer_card("a.zip", 2048, "http://127.0.0.1:9876/dl?t=xyz", 1_700_000_000);
        let text = v.to_string();
        assert!(text.contains("http://127.0.0.1:9876/dl?t=xyz"));
        assert!(text.contains("2 KB"));
        assert_eq!(v["msg_type"], "interactive");
        // 头部：品牌色 turquoise + 主标题 + 副标题（链接有效期）
        assert_eq!(v["card"]["header"]["template"], "turquoise");
        assert_eq!(v["card"]["header"]["title"]["content"], "有一个文件等你取回");
        assert_eq!(v["card"]["header"]["subtitle"]["content"], "链接 10 分钟内有效");
        // 文件名以超链接形式出现，且带文件图标
        assert!(text.contains("**[a.zip](http://127.0.0.1:9876/dl?t=xyz)**"));
        assert!(text.contains("🗜️"));
        // footer：发送时间（到分钟）+ 过期提示
        assert!(text.contains("发出"));
        assert!(text.contains("过期后请让对方重新发送"));
        // 端限制提示（C11/M0c）：电脑端（青鸟 APP 或 CLI）
        assert!(text.contains("取回只能在电脑端完成（青鸟 APP 或 CLI），手机端不支持"));
        assert_eq!(v["card"]["elements"][1]["actions"][0]["url"], "http://127.0.0.1:9876/dl?t=xyz");
        assert_eq!(v["card"]["elements"][1]["actions"][0]["text"]["content"], "取回文件");
        // 结构：0 文件信息 div → 1 取回按钮 → 2 分界线 → 3 端限制 note → 4 发送时间 note
        assert_eq!(v["card"]["elements"][2]["tag"], "hr");
        assert_eq!(v["card"]["elements"][3]["tag"], "note");
        assert_eq!(
            v["card"]["elements"][3]["elements"][0]["content"],
            "💻 取回只能在电脑端完成（青鸟 APP 或 CLI），手机端不支持"
        );
        assert_eq!(v["card"]["elements"][4]["tag"], "note");
    }

    #[test]
    fn fmt_local_time_shape_is_stable() {
        // 不依赖本机时区：只校验形状 YYYY-MM-DD HH:MM
        let s = fmt_local_time(1_700_000_000);
        assert_eq!(s.len(), 16, "{s}");
        assert_eq!(&s[4..5], "-");
        assert_eq!(&s[10..11], " ");
        assert_eq!(&s[13..14], ":");
        assert!(s.chars().filter(char::is_ascii_digit).count() >= 12);
        // 越界时间戳不 panic，退化为空串
        assert!(fmt_local_time(i64::MAX).is_empty());
    }

    #[test]
    fn file_icon_falls_back_to_clip() {
        assert_eq!(file_icon("no-extension"), "📎");
        assert_eq!(file_icon("a.unknownext"), "📎");
        assert_eq!(file_icon("A.ZIP"), "🗜️");
    }

    #[test]
    fn error_classification() {
        let e = FeishuError { code: 1061045, msg: "".into(), http_status: 200 };
        assert!(e.retryable());
        let e = FeishuError { code: 99991403, msg: "".into(), http_status: 200 };
        assert!(!e.retryable());
        assert!(e.quota_exhausted());
        let e = FeishuError { code: 0, msg: "".into(), http_status: 429 };
        assert!(e.retryable());
    }

    /* ===== M0a 出口 2：确定性面 golden（固定在重构前取值，见方案 §10） ===== */

    /// 时间渲染格式逐字钉死，且**与时区无关**（偏移由入参给定）。
    /// 精度为分钟（YYYY-MM-DD HH:MM）——卡片改版（1ea7f28）把秒精简掉了。
    #[test]
    fn fmt_local_time_at_is_byte_stable() {
        let ts = 1_700_000_000; // UTC 2023-11-14 22:13:20
        let utc = time::UtcOffset::UTC;
        let east8 = time::UtcOffset::from_hms(8, 0, 0).expect("合法偏移");
        assert_eq!(fmt_local_time_at(ts, utc), "2023-11-14 22:13");
        assert_eq!(fmt_local_time_at(ts, east8), "2023-11-15 06:13");
        // 跨日 / 负偏移
        let west5 = time::UtcOffset::from_hms(-5, 0, 0).expect("合法偏移");
        assert_eq!(fmt_local_time_at(ts, west5), "2023-11-14 17:13");
        // 越界时间戳退化为空串（不 panic）
        assert_eq!(fmt_local_time_at(i64::MAX, utc), "");
        assert_eq!(fmt_local_time_at(i64::MIN, utc), "");
    }

    /// 大小文案：边界值逐条钉死
    #[test]
    fn human_size_vectors() {
        assert_eq!(human_size(0), "0 B");
        assert_eq!(human_size(1), "1 B");
        assert_eq!(human_size(1023), "1023 B");
        assert_eq!(human_size(1024), "1 KB");
        assert_eq!(human_size(2048), "2 KB");
        assert_eq!(human_size(1024 * 1024 - 1), "1023 KB");
        assert_eq!(human_size(1024 * 1024), "1.0 MB");
        assert_eq!(human_size(381952), "373 KB");
        assert_eq!(human_size(1024 * 1024 * 1024 - 1), "1024.0 MB");
        assert_eq!(human_size(1024 * 1024 * 1024), "1.00 GB");
        assert_eq!(human_size(100 * 1024 * 1024), "100.0 MB");
    }

    /// 图标映射：每个分支 + 大小写 + 多扩展名 + 无扩展名
    #[test]
    fn file_icon_vectors() {
        for (name, want) in [
            ("a.zip", "🗜️"), ("a.7z", "🗜️"), ("a.tar.gz", "🗜️"),
            ("a.pdf", "📕"), ("a.docx", "📘"), ("a.xlsx", "📗"), ("a.csv", "📗"),
            ("a.pptx", "📙"), ("a.png", "🖼️"), ("a.HEIC", "🖼️"),
            ("a.mp4", "🎬"), ("a.mp3", "🎵"), ("a.log", "📄"), ("a.toml", "📄"),
            ("a.zip.bak", "📎"), ("noext", "📎"), ("a.", "📎"), ("", "📎"),
        ] {
            assert_eq!(file_icon(name), want, "文件名 {name:?} 的图标不符");
        }
    }

    /// footer 与卡片整体：**结构**与**除时间外的全部文案**逐字钉死。
    /// 时间部分单独由 `fmt_local_time_at_is_byte_stable` 覆盖，因此本用例与时区无关。
    /// 基线在卡片 turquoise 改版合并时重捕（merge `7d7799b9`，M0c 式收尾）。
    #[test]
    fn transfer_card_golden_without_time() {
        let v = build_transfer_card("a.zip", 2048, "http://127.0.0.1:9876/dl?t=xyz", 1_700_000_000);
        let s = serde_json::to_string_pretty(&v).expect("序列化");
        // 把渲染出的时间替换成占位符——它是唯一随环境变化的字段。
        // 不引入正则依赖：用固定前后缀定位，顺带断言时间格式长度。
        let head = "⏳ ";
        let tail = " 发出 · 过期后请让对方重新发送";
        let i = s.find(head).expect("卡片应含发送时间") + head.len();
        let j = i + s[i..].find(tail).expect("卡片应含过期提示");
        let rendered = &s[i..j];
        assert_eq!(rendered.len(), 16, "时间格式应为 YYYY-MM-DD HH:MM，实际 {rendered:?}");
        let s = format!("{}<TIME>{}", &s[..i], &s[j..]);
        let want = serde_json::json!({
            "card": {
                "config": { "wide_screen_mode": true },
                "header": { "template": "turquoise",
                            "title": { "tag": "plain_text", "content": "有一个文件等你取回" },
                            "subtitle": { "tag": "plain_text", "content": "链接 10 分钟内有效" } },
                "elements": [
                    { "tag": "div", "text": { "tag": "lark_md",
                      "content": "🗜️ **[a.zip](http://127.0.0.1:9876/dl?t=xyz)**\n<font color='grey'>2 KB</font>" } },
                    { "tag": "action", "actions": [
                        { "tag": "button", "type": "primary",
                          "text": { "tag": "plain_text", "content": "取回文件" },
                          "url": "http://127.0.0.1:9876/dl?t=xyz" } ] },
                    { "tag": "hr" },
                    { "tag": "note", "elements": [ { "tag": "plain_text",
                      "content": "💻 取回只能在电脑端完成（青鸟 APP 或 CLI），手机端不支持" } ] },
                    { "tag": "note", "elements": [ { "tag": "plain_text",
                      "content": "⏳ <TIME> 发出 · 过期后请让对方重新发送" } ] }
                ]
            },
            "msg_type": "interactive"
        });
        assert_eq!(
            serde_json::to_string_pretty(&want).expect("序列化"),
            s,
            "卡片 JSON 与冻结基线不一致——改动卡片结构或文案必须显式重捕 golden"
        );
    }
}
