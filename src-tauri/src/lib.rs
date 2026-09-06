// 青鸟 · 飞书消息助手 — Tauri 后端
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use tauri::Manager;

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

/// 配置文件路径：<app_config_dir>/qingniao.json
fn config_path(app: &tauri::AppHandle) -> Result<PathBuf, String> {
    let dir = app
        .path()
        .app_config_dir()
        .map_err(|e| format!("无法定位配置目录: {e}"))?;
    std::fs::create_dir_all(&dir).map_err(|e| format!("无法创建配置目录: {e}"))?;
    Ok(dir.join("qingniao.json"))
}

/// 读取配置
#[tauri::command]
fn load_config(app: tauri::AppHandle) -> Result<AppConfig, String> {
    let path = config_path(&app)?;
    if !path.exists() {
        return Ok(AppConfig::default());
    }
    let data = std::fs::read_to_string(&path).map_err(|e| format!("读取配置失败: {e}"))?;
    serde_json::from_str(&data).map_err(|e| format!("配置解析失败: {e}"))
}

/// 保存配置（原子写入）
#[tauri::command]
fn save_config(app: tauri::AppHandle, config: AppConfig) -> Result<(), String> {
    let path = config_path(&app)?;
    let data = serde_json::to_string_pretty(&config).map_err(|e| format!("配置序列化失败: {e}"))?;
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, data).map_err(|e| format!("写入配置失败: {e}"))?;
    std::fs::rename(tmp, path).map_err(|e| format!("保存配置失败: {e}"))
}

/// 向飞书 webhook 发送消息
#[tauri::command]
fn send_webhook(url: String, payload: serde_json::Value) -> Result<String, String> {
    let trimmed = url.trim().to_string();
    if !trimmed.starts_with("https://") && !trimmed.starts_with("http://") {
        return Err("Webhook 地址必须以 http:// 或 https:// 开头".into());
    }
    let client = reqwest::blocking::Client::builder()
        .timeout(std::time::Duration::from_secs(15))
        .build()
        .map_err(|e| format!("创建 HTTP 客户端失败: {e}"))?;
    let resp = client
        .post(&trimmed)
        .json(&payload)
        .send()
        .map_err(|e| format!("请求失败: {e}"))?;
    let status = resp.status();
    let body = resp.text().map_err(|e| format!("读取响应失败: {e}"))?;
    Ok(format!("HTTP {}: {}", status.as_u16(), body))
}

/// 上传图片到飞书，返回 image_key
/// 流程：用 app_id/app_secret 换 tenant_access_token → multipart 上传到 im/v1/images
#[tauri::command]
fn upload_image(
    image_base64: String,
    filename: String,
    app_id: String,
    app_secret: String,
) -> Result<String, String> {
    use base64::Engine;

    if app_id.is_empty() || app_secret.is_empty() {
        return Err("未配置飞书应用凭证（App ID / App Secret）".into());
    }

    // 解码 base64
    let image_bytes = base64::engine::general_purpose::STANDARD
        .decode(image_base64.trim())
        .map_err(|e| format!("图片解码失败: {e}"))?;

    let client = reqwest::blocking::Client::builder()
        .timeout(std::time::Duration::from_secs(30))
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
        .map_err(|e| format!("获取 token 请求失败: {e}"))?;
    let token_json: serde_json::Value = token_resp
        .json()
        .map_err(|e| format!("token 响应解析失败: {e}"))?;
    if token_json["code"].as_i64().unwrap_or(-1) != 0 {
        return Err(format!(
            "获取 token 失败: {} {}",
            token_json["code"], token_json["msg"]
        ));
    }
    let token = token_json["tenant_access_token"]
        .as_str()
        .ok_or("token 响应中缺少 tenant_access_token")?
        .to_string();

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
        .map_err(|e| format!("上传图片请求失败: {e}"))?;
    let upload_json: serde_json::Value = upload_resp
        .json()
        .map_err(|e| format!("上传响应解析失败: {e}"))?;

    if upload_json["code"].as_i64().unwrap_or(-1) != 0 {
        return Err(format!(
            "上传图片失败: {} {}",
            upload_json["code"], upload_json["msg"]
        ));
    }

    let image_key = upload_json["data"]["image_key"]
        .as_str()
        .ok_or("上传响应中缺少 image_key")?
        .to_string();

    Ok(image_key)
}

/// 测试飞书应用凭证：换取 tenant_access_token 并读取机器人信息，返回应用名
#[tauri::command]
fn test_connection(app_id: String, app_secret: String) -> Result<String, String> {
    let app_id = app_id.trim();
    let app_secret = app_secret.trim();
    if app_id.is_empty() || app_secret.is_empty() {
        return Err("未配置飞书应用凭证（App ID / App Secret）".into());
    }

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
        .map_err(|e| format!("获取 token 请求失败: {e}"))?;
    let token_json: serde_json::Value = token_resp
        .json()
        .map_err(|e| format!("token 响应解析失败: {e}"))?;
    if token_json["code"].as_i64().unwrap_or(-1) != 0 {
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
        .map_err(|e| format!("读取机器人信息失败: {e}"))?;
    let info_json: serde_json::Value = info_resp
        .json()
        .map_err(|e| format!("机器人信息响应解析失败: {e}"))?;
    if info_json["code"].as_i64().unwrap_or(-1) != 0 {
        return Err(format!(
            "连接失败: {} {}",
            info_json["code"], info_json["msg"]
        ));
    }

    Ok(info_json["bot"]["app_name"]
        .as_str()
        .unwrap_or("飞书应用")
        .to_string())
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
            test_connection
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
