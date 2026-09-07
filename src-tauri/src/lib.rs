// 青鸟 · 飞书消息助手 — Tauri 后端
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::io::Write;
use tauri::Manager;

/// 日志级别
const LV_DEBUG: &str = "debug";
const LV_INFO: &str = "info";
const LV_WARN: &str = "warn";
const LV_ERROR: &str = "error";

/// 日志文件路径：与配置文件同目录（<app_config_dir>/qingniao.log）
fn log_path(app: &tauri::AppHandle) -> Result<PathBuf, String> {
    let dir = app
        .path()
        .app_config_dir()
        .map_err(|e| format!("无法定位配置目录: {e}"))?;
    Ok(dir.join("qingniao.log"))
}

/// 当前时间（本地时区），格式如 2026-09-07 14:23:45
fn now_str() -> String {
    let now = time::OffsetDateTime::now_local().unwrap_or_else(|_| time::OffsetDateTime::now_utc());
    format!(
        "{:04}-{:02}-{:02} {:02}:{:02}:{:02}",
        now.year(), now.month() as u8, now.day(),
        now.hour(), now.minute(), now.second()
    )
}

/// 追加一行日志到 <配置目录>/qingniao.log（写失败不影响主流程）
fn write_log(app: &tauri::AppHandle, level: &str, msg: &str) {
    let Ok(path) = log_path(app) else { return };
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(&path) {
        let _ = writeln!(f, "[{}] [{}] {}", now_str(), level.to_uppercase(), msg);
    }
}

/// 把错误及其完整 source 链拼成一行，便于定位网络层具体原因（超时/连接重置/DNS 等）
fn err_chain(e: &dyn std::error::Error) -> String {
    let mut out = e.to_string();
    let mut cur = e.source();
    while let Some(src) = cur {
        out.push_str(" <- ");
        out.push_str(&src.to_string());
        cur = src.source();
    }
    out
}

/// 前端日志通道：让 JS 侧把控制台/异常信息写入同一个日志文件
#[tauri::command]
fn log_event(app: tauri::AppHandle, level: String, message: String) {
    let level = match level.as_str() {
        "debug" | "info" | "warn" | "error" => level.as_str(),
        _ => "info",
    };
    write_log(&app, level, &message);
}

/// 隐藏 app_id 的中段，只保留前后几位的脱敏形式
fn mask_app_id(app_id: &str) -> String {
    let id: Vec<char> = app_id.chars().collect();
    if id.len() <= 8 {
        return "****".to_string();
    }
    format!("{}{}{}", id[..4].iter().collect::<String>(), "…", id[id.len()-4..].iter().collect::<String>())
}

/// 脱敏 webhook 地址：隐藏 hook 密钥段
fn mask_webhook(url: &str) -> String {
    match url.find("hook/") {
        Some(i) => format!("{}hook/****", &url[..i]),
        None => url.to_string(),
    }
}

/// 一个已保存的 webhook 机器人
#[derive(Serialize, Deserialize, Default, Clone)]
pub struct WebhookItem {
    pub name: String,
    pub url: String,
    pub secret: String,
}

/// 一条发送历史
#[derive(Serialize, Deserialize, Default, Clone)]
pub struct HistoryItem {
    pub time: String,
    pub msg_type: String,
    pub summary: String,
    pub payload: serde_json::Value,
    #[serde(default)]
    pub ok: bool,
    #[serde(default)]
    pub status: String,
}

/// 应用配置
#[derive(Serialize, Deserialize, Default, Clone)]
pub struct AppConfig {
    pub webhooks: Vec<WebhookItem>,
    pub history: Vec<HistoryItem>,
    pub last_webhook: Option<usize>,
    pub last_type: Option<String>,
    #[serde(default)]
    pub app_id: String,
    #[serde(default)]
    pub app_secret: String,
}

/// 旧版配置目录（identifier 曾为 com.qingniao.app）——仅用于一次性迁移
const OLD_CONFIG_DIR: &str = "com.qingniao.app";

/// 配置文件路径：<app_config_dir>/qingniao.json（目录名为 qingniao）
fn config_path(app: &tauri::AppHandle) -> Result<PathBuf, String> {
    let dir = app
        .path()
        .app_config_dir()
        .map_err(|e| format!("无法定位配置目录: {e}"))?;
    std::fs::create_dir_all(&dir).map_err(|e| format!("无法创建配置目录: {e}"))?;
    let path = dir.join("qingniao.json");
    migrate_old_config(app, &path)?;
    Ok(path)
}

/// 一次性迁移：新版配置目录改为 qingniao 后，若旧目录 com.qingniao.app 中
/// 存在配置文件且新位置还没有，则拷贝过来（保留旧文件作为备份，不删除）。
fn migrate_old_config(app: &tauri::AppHandle, new_path: &Path) -> Result<(), String> {
    if new_path.exists() {
        return Ok(());
    }
    let old_dir = app
        .path()
        .config_dir()
        .map_err(|e| format!("无法定位系统配置目录: {e}"))?
        .join(OLD_CONFIG_DIR);
    let old_path = old_dir.join("qingniao.json");
    if !old_path.exists() {
        return Ok(());
    }
    std::fs::copy(&old_path, new_path).map_err(|e| format!("迁移旧配置失败: {e}"))?;
    write_log(app, LV_INFO, &format!("已从旧目录迁移配置: {} → {}", old_path.display(), new_path.display()));
    Ok(())
}

/// 读取配置
#[tauri::command]
fn load_config(app: tauri::AppHandle) -> Result<AppConfig, String> {
    let path = config_path(&app)?;
    write_log(&app, LV_DEBUG, &format!("load_config: {}", path.display()));
    if !path.exists() {
        write_log(&app, LV_INFO, "load_config: 配置文件不存在，返回默认配置");
        return Ok(AppConfig::default());
    }
    let data = std::fs::read_to_string(&path).map_err(|e| {
        write_log(&app, LV_ERROR, &format!("load_config 读取失败: {e}"));
        format!("读取配置失败: {e}")
    })?;
    match serde_json::from_str(&data) {
        Ok(cfg) => {
            write_log(&app, LV_INFO, &format!("load_config: 成功（{} 字节）", data.len()));
            Ok(cfg)
        }
        Err(e) => {
            write_log(&app, LV_ERROR, &format!("load_config 解析失败: {e}"));
            Err(format!("配置解析失败: {e}"))
        }
    }
}

/// 保存配置（原子写入）
#[tauri::command]
fn save_config(app: tauri::AppHandle, config: AppConfig) -> Result<(), String> {
    let path = config_path(&app)?;
    let data = serde_json::to_string_pretty(&config).map_err(|e| format!("配置序列化失败: {e}"))?;
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, &data).map_err(|e| {
        write_log(&app, LV_ERROR, &format!("save_config 写入临时文件失败: {e}"));
        format!("写入配置失败: {e}")
    })?;
    std::fs::rename(tmp, &path).map_err(|e| {
        write_log(&app, LV_ERROR, &format!("save_config 重命名失败: {e}"));
        format!("保存配置失败: {e}")
    })?;
    write_log(&app, LV_INFO, &format!("save_config: 已保存 {} 个机器人 / {} 条历史", config.webhooks.len(), config.history.len()));
    Ok(())
}

/// 向飞书 webhook 发送消息
#[tauri::command]
fn send_webhook(app: tauri::AppHandle, url: String, payload: serde_json::Value) -> Result<String, String> {
    let trimmed = url.trim().to_string();
    if !trimmed.starts_with("https://") && !trimmed.starts_with("http://") {
        write_log(&app, LV_WARN, "send_webhook: 地址不以 http(s) 开头，已拒绝");
        return Err("Webhook 地址必须以 http:// 或 https:// 开头".into());
    }
    let msg_type = payload.get("msg_type").and_then(|v| v.as_str()).unwrap_or("?");
    write_log(&app, LV_INFO, &format!("send_webhook: 开始发送 msg_type={} → {}", msg_type, mask_webhook(&trimmed)));
    let client = reqwest::blocking::Client::builder()
        .timeout(std::time::Duration::from_secs(15))
        .build()
        .map_err(|e| format!("创建 HTTP 客户端失败: {e}"))?;
    let resp = client
        .post(&trimmed)
        .json(&payload)
        .send()
        .map_err(|e| {
            write_log(&app, LV_ERROR, &format!("send_webhook 请求失败: {}", err_chain(&e)));
            format!("请求失败: {e}")
        })?;
    let status = resp.status();
    let body = resp.text().map_err(|e| format!("读取响应失败: {e}"))?;
    write_log(&app, LV_INFO, &format!("send_webhook: 完成 HTTP {} body={}", status.as_u16(), body));
    Ok(format!("HTTP {}: {}", status.as_u16(), body))
}

/// 上传图片到飞书，返回 image_key
/// 流程：用 app_id/app_secret 换 tenant_access_token → multipart 上传到 im/v1/images
#[tauri::command]
fn upload_image(
    app: tauri::AppHandle,
    image_base64: String,
    filename: String,
    app_id: String,
    app_secret: String,
) -> Result<String, String> {
    use base64::Engine;

    if app_id.is_empty() || app_secret.is_empty() {
        write_log(&app, LV_WARN, "upload_image: 未配置飞书应用凭证（App ID / App Secret）");
        return Err("未配置飞书应用凭证（App ID / App Secret）".into());
    }
    write_log(&app, LV_INFO, &format!(
        "upload_image: 开始上传 filename={} app_id={} base64长度={}",
        filename, mask_app_id(&app_id), image_base64.len()
    ));

    // 解码 base64
    let image_bytes = base64::engine::general_purpose::STANDARD
        .decode(image_base64.trim())
        .map_err(|e| {
            write_log(&app, LV_ERROR, &format!("upload_image 图片解码失败: {e}"));
            format!("图片解码失败: {e}")
        })?;
    write_log(&app, LV_DEBUG, &format!("upload_image: 解码成功，图片大小 {} 字节", image_bytes.len()));

    let client = reqwest::blocking::Client::builder()
        .timeout(std::time::Duration::from_secs(60))
        .build()
        .map_err(|e| format!("创建 HTTP 客户端失败: {e}"))?;

    // 1. 获取 tenant_access_token
    let token_resp = client
        .post("https://open.feishu.cn/open-apis/auth/v3/tenant_access_token/internal")
        .json(&serde_json::json!({
            "app_id": app_id,
            "app_secret": app_secret
        }))
        .send()
        .map_err(|e| {
            write_log(&app, LV_ERROR, &format!("upload_image 获取 token 请求失败: {}", err_chain(&e)));
            format!("获取 token 请求失败: {e}")
        })?;
    let token_json: serde_json::Value = token_resp
        .json()
        .map_err(|e| format!("token 响应解析失败: {e}"))?;
    if token_json["code"].as_i64().unwrap_or(-1) != 0 {
        write_log(&app, LV_ERROR, &format!(
            "upload_image 获取 token 失败: code={} msg={}",
            token_json["code"], token_json["msg"]
        ));
        return Err(format!(
            "获取 token 失败: {} {}",
            token_json["code"], token_json["msg"]
        ));
    }
    let token = token_json["tenant_access_token"]
        .as_str()
        .ok_or("token 响应中缺少 tenant_access_token")?
        .to_string();
    write_log(&app, LV_DEBUG, "upload_image: 获取 tenant_access_token 成功");

    // 2. multipart 上传图片
    let part = reqwest::blocking::multipart::Part::bytes(image_bytes)
        .file_name(filename.clone());
    let form = reqwest::blocking::multipart::Form::new()
        .text("image_type", "message")
        .part("image", part);

    let upload_resp = client
        .post("https://open.feishu.cn/open-apis/im/v1/images")
        .bearer_auth(token)
        .multipart(form)
        .send()
        .map_err(|e| {
            write_log(&app, LV_ERROR, &format!("upload_image 上传请求失败: {}", err_chain(&e)));
            format!("上传图片请求失败: {e}")
        })?;
    let upload_json: serde_json::Value = upload_resp
        .json()
        .map_err(|e| format!("上传响应解析失败: {e}"))?;

    if upload_json["code"].as_i64().unwrap_or(-1) != 0 {
        write_log(&app, LV_ERROR, &format!(
            "upload_image 飞书返回失败: code={} msg={}",
            upload_json["code"], upload_json["msg"]
        ));
        return Err(format!(
            "上传图片失败: {} {}",
            upload_json["code"], upload_json["msg"]
        ));
    }

    let image_key = upload_json["data"]["image_key"]
        .as_str()
        .ok_or("上传响应中缺少 image_key")?
        .to_string();
    write_log(&app, LV_INFO, &format!("upload_image: 上传成功 image_key={}", image_key));

    Ok(image_key)
}

/// 测试飞书应用凭证：换取 tenant_access_token 并读取机器人信息，返回应用名
#[tauri::command]
fn test_connection(app: tauri::AppHandle, app_id: String, app_secret: String) -> Result<String, String> {
    let app_id = app_id.trim().to_string();
    let app_secret = app_secret.trim().to_string();
    if app_id.is_empty() || app_secret.is_empty() {
        write_log(&app, LV_WARN, "test_connection: 未配置飞书应用凭证");
        return Err("未配置飞书应用凭证（App ID / App Secret）".into());
    }
    write_log(&app, LV_INFO, &format!("test_connection: 开始测试 app_id={}", mask_app_id(&app_id)));

    let client = reqwest::blocking::Client::builder()
        .timeout(std::time::Duration::from_secs(15))
        .build()
        .map_err(|e| format!("创建 HTTP 客户端失败: {e}"))?;

    // 1. 获取 tenant_access_token
    let token_resp = client
        .post("https://open.feishu.cn/open-apis/auth/v3/tenant_access_token/internal")
        .json(&serde_json::json!({
            "app_id": app_id,
            "app_secret": app_secret
        }))
        .send()
        .map_err(|e| {
            write_log(&app, LV_ERROR, &format!("test_connection 获取 token 请求失败: {}", err_chain(&e)));
            format!("获取 token 请求失败: {e}")
        })?;
    let token_json: serde_json::Value = token_resp
        .json()
        .map_err(|e| format!("token 响应解析失败: {e}"))?;
    if token_json["code"].as_i64().unwrap_or(-1) != 0 {
        write_log(&app, LV_ERROR, &format!(
            "test_connection 获取 token 失败: code={} msg={}",
            token_json["code"], token_json["msg"]
        ));
        return Err(format!(
            "获取 token 失败: {} {}",
            token_json["code"], token_json["msg"]
        ));
    }
    let token = token_json["tenant_access_token"]
        .as_str()
        .ok_or("token 响应中缺少 tenant_access_token")?
        .to_string();

    // 2. 读取机器人信息，验证 token 可用并取应用名
    let info_resp = client
        .get("https://open.feishu.cn/open-apis/bot/v3/info")
        .bearer_auth(&token)
        .send()
        .map_err(|e| {
            write_log(&app, LV_ERROR, &format!("test_connection 读取机器人信息失败: {}", err_chain(&e)));
            format!("读取机器人信息失败: {e}")
        })?;
    let info_json: serde_json::Value = info_resp
        .json()
        .map_err(|e| format!("机器人信息响应解析失败: {e}"))?;
    if info_json["code"].as_i64().unwrap_or(-1) != 0 {
        write_log(&app, LV_ERROR, &format!(
            "test_connection 失败: code={} msg={}",
            info_json["code"], info_json["msg"]
        ));
        return Err(format!(
            "连接失败: {} {}",
            info_json["code"], info_json["msg"]
        ));
    }

    let app_name = info_json["bot"]["app_name"]
        .as_str()
        .unwrap_or("飞书应用")
        .to_string();
    write_log(&app, LV_INFO, &format!("test_connection: 连接成功 app_name={}", app_name));
    Ok(app_name)
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        .plugin(tauri_plugin_opener::init())
        .invoke_handler(tauri::generate_handler![
            load_config,
            save_config,
            send_webhook,
            upload_image,
            test_connection,
            log_event
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
