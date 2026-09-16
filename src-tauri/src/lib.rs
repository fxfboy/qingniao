// 青鸟 · 飞书消息助手 — Tauri 后端
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::io::Write;
use tauri::Manager;

/// 常驻功能的模块划分（设计文档 dock/tray 常驻功能设计文档 v0.4）
pub mod lifecycle;
pub mod service;
pub mod tray;

use lifecycle::{ExitCoordinator, ExitState, FinalCleanup, QuitSource, QuitStep};
use service::{
    LocalServiceController, LocalServiceStatus, PhaseAService, ServiceStatusProvider,
    TransferSnapshot,
};

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

/// 向指定文件追加一行日志（写失败不影响主流程）
fn append_log_line(path: &Path, level: &str, msg: &str) {
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(path) {
        let _ = writeln!(f, "[{}] [{}] {}", now_str(), level.to_uppercase(), msg);
    }
}

/// 追加一行日志到 <配置目录>/qingniao.log（写失败不影响主流程）
fn write_log(app: &tauri::AppHandle, level: &str, msg: &str) {
    if let Ok(path) = log_path(app) {
        append_log_line(&path, level, msg);
    }
}

/// 把 `log` crate 的记录转发到与应用同一份日志文件。
///
/// **为什么需要它**：Tauri 核心不安装 logger，`log::info!` 之类在默认情况下是
/// 彻底的 no-op（不会报错，也不会落盘）。常驻功能的 `lifecycle` / `tray` / `service`
/// 模块刻意不依赖 `AppHandle`（以便脱离 Tauri 单测），因此无法直接调用 `write_log`；
/// 改为安装这个全局 logger，让 `log::*` 与 `write_log` 落到同一份
/// `<配置目录>/qingniao.log`。
struct FileLogger {
    path: PathBuf,
}

impl log::Log for FileLogger {
    fn enabled(&self, _metadata: &log::Metadata) -> bool {
        true
    }

    fn log(&self, record: &log::Record) {
        let level = match record.level() {
            log::Level::Error => LV_ERROR,
            log::Level::Warn => LV_WARN,
            log::Level::Info => LV_INFO,
            log::Level::Debug | log::Level::Trace => LV_DEBUG,
        };
        append_log_line(&self.path, level, &record.args().to_string());
    }

    fn flush(&self) {}
}

/// 安装文件 logger。在 `setup` 中、任何会写日志的初始化之前调用一次。
fn install_file_logger(app: &tauri::AppHandle) {
    let Ok(path) = log_path(app) else { return };
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    // 已安装过时 set_boxed_logger 返回 Err，忽略即可。
    if log::set_boxed_logger(Box::new(FileLogger { path })).is_ok() {
        // 默认记到 info；debug/trace 不进文件，避免刷屏。
        log::set_max_level(log::LevelFilter::Info);
    }
}

/// **仅 debug 构建**：驱动本地服务状态，用于验收 A8（状态刷新）。
///
/// 阶段 A 没有真实 listener，服务状态是静态的；没有这个驱动入口，
/// 「状态迁移 → 菜单文案更新」这条链路无法在运行中的应用里被观察到。
/// release 构建不含该命令（见 `generate_handler!` 上的 `#[cfg]`）。
///
/// 用法：`invoke('debug_set_service_status', { status: 'running:12345' })`
/// 取值：`starting` / `running:<port>` / `stopping` / `stopped` / `port_in_use:<port>`
#[cfg(debug_assertions)]
#[tauri::command]
fn debug_set_service_status(app: tauri::AppHandle, status: String) {
    let parsed = match status.split_once(':') {
        Some(("running", port)) => LocalServiceStatus::Running {
            bound_port: port.parse().unwrap_or(service::DEFAULT_LOCAL_PORT),
        },
        Some(("port_in_use", port)) => LocalServiceStatus::Failed {
            kind: service::ServiceFailure::PortInUse,
            requested_port: port.parse().unwrap_or(service::DEFAULT_LOCAL_PORT),
        },
        _ => match status.as_str() {
            "starting" => LocalServiceStatus::Starting,
            "stopping" => LocalServiceStatus::Stopping,
            "stopped" => LocalServiceStatus::Stopped,
            other => {
                log::warn!("debug_set_service_status: 未知状态 {other}");
                return;
            }
        },
    };
    log::info!("debug_set_service_status: {parsed:?}");
    // 走唯一推送点：更新 provider + 刷新菜单（§5.2）
    app.state::<AppState>().set_service_status(parsed);
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

/// 连接测试结果
#[derive(Serialize)]
struct ConnTestResult {
    app_name: String,
    has_upload_permission: bool,
    apply_url: Option<String>,
}

/// 1x1 透明 PNG，用于探测 im:resource:upload 权限（极小，不产生实际影响）
const PROBE_PNG_BASE64: &str = "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mNk+M9QDwADhgGAWjR9awAAAABJRU5ErkJggg==";

/// 从飞书权限不足的错误消息中提取申请链接，提取不到则按格式构造
fn extract_apply_url(msg: &str, app_id: &str) -> String {
    if let Some(start) = msg.find("https://open.feishu.cn/app/") {
        let rest = &msg[start..];
        let end = rest
            .find(|c: char| c.is_whitespace() || c == '"' || c == '）' || c == ')' || c == '，')
            .unwrap_or(rest.len());
        return rest[..end].to_string();
    }
    format!(
        "https://open.feishu.cn/app/{}/auth?q=im:resource:upload&op_from=openapi&token_type=tenant",
        app_id
    )
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

/// 测试飞书应用凭证：换取 tenant_access_token → 读取机器人信息 → 探测 im:resource:upload 权限
#[tauri::command]
fn test_connection(app: tauri::AppHandle, app_id: String, app_secret: String) -> Result<ConnTestResult, String> {
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

    // 3. 探测 im:resource:upload 权限：上传一张 1x1 透明 PNG
    let has_upload_permission;
    let apply_url;
    {
        use base64::Engine;
        let probe_bytes = base64::engine::general_purpose::STANDARD
            .decode(PROBE_PNG_BASE64)
            .map_err(|e| format!("探测图片解码失败: {e}"))?;
        let part = reqwest::blocking::multipart::Part::bytes(probe_bytes).file_name("probe.png");
        let form = reqwest::blocking::multipart::Form::new()
            .text("image_type", "message")
            .part("image", part);
        let probe_resp = client
            .post("https://open.feishu.cn/open-apis/im/v1/images")
            .bearer_auth(&token)
            .multipart(form)
            .send()
            .map_err(|e| {
                write_log(&app, LV_ERROR, &format!("test_connection 探测上传权限请求失败: {}", err_chain(&e)));
                format!("探测上传权限失败: {e}")
            })?;
        let probe_json: serde_json::Value = probe_resp
            .json()
            .map_err(|e| format!("探测响应解析失败: {e}"))?;
        if probe_json["code"].as_i64().unwrap_or(-1) == 0 {
            has_upload_permission = true;
            apply_url = None;
            write_log(&app, LV_INFO, "test_connection: im:resource:upload 权限已开通");
        } else {
            has_upload_permission = false;
            let msg = probe_json["msg"].as_str().unwrap_or("");
            apply_url = Some(extract_apply_url(msg, &app_id));
            write_log(&app, LV_WARN, &format!(
                "test_connection: 缺少 im:resource:upload 权限 code={} msg={}",
                probe_json["code"], probe_json["msg"]
            ));
        }
    }

    Ok(ConnTestResult {
        app_name,
        has_upload_permission,
        apply_url,
    })
}

// ---------------------------------------------------------------------------
// 常驻功能：应用状态、窗口恢复、菜单分派、退出协议、启动装配
// ---------------------------------------------------------------------------

/// 应用级共享状态（§5.2：Rust `AppState` 持有服务状态、传输快照与菜单项句柄）
pub struct AppState {
    pub exit: ExitCoordinator,
    pub service: PhaseAService,
    /// 状态项显示端。存为 trait object 以便单测注入替身（A8 的 `unit/fake` 验收）
    pub tray: std::sync::Mutex<Option<std::sync::Arc<dyn tray::StatusDisplay>>>,}

impl AppState {
    fn new() -> Self {
        Self {
            exit: ExitCoordinator::default(),
            service: PhaseAService::new(),
            tray: std::sync::Mutex::new(None),
        }
    }

    /// 服务状态迁移的**唯一推送点**（§5.2）。
    ///
    /// 先更新权威 provider，再刷新菜单项文字。任何改变服务状态的路径都必须走这里，
    /// 否则菜单会一直显示陈旧状态——这正是「`refresh_status` 有定义但零调用」的成因。
    pub fn set_service_status(&self, status: LocalServiceStatus) {
        self.service.service.set_status(status);
        self.sync_tray_status();
    }

    /// 按当前权威状态刷新常驻菜单的状态项（文案端口一律取自状态本身，§9.2）
    pub fn sync_tray_status(&self) {
        let label = self.service.service.snapshot().menu_label();
        if let Some(display) = self.tray.lock().unwrap().as_ref() {
            display.show_status(&label);
        }
    }
}

/// 恢复并聚焦主窗口（D2：单窗口隐藏复用，正常路径**不**重建）
pub fn show_main_window(app: &tauri::AppHandle) {
    match app.get_webview_window("main") {
        Some(window) => {
            let _ = window.unminimize();
            let _ = window.show();
            let _ = window.set_focus();
            // 与 CloseRequested 隐藏路径配对：恢复任务栏/Dock 图标
            #[cfg(target_os = "windows")]
            let _ = window.set_skip_taskbar(false);
            #[cfg(target_os = "macos")]
            tray::apply_dock_policy(app, true);
        }
        None => log::warn!("主窗口不存在，无法恢复"),
    }
}

/// 菜单事件分派：一律按**稳定 `MenuId`**，不得用展示文字（§5.1）
pub fn menu_action(app: &tauri::AppHandle, id: &str) {
    match id {
        tray::ID_OPEN_MAIN => show_main_window(app),
        tray::ID_OPEN_DOWNLOADS => open_download_dir(app),
        // 只读 / 未启用项不应产生事件
        tray::ID_SERVICE_STATUS => log::warn!("service_status 为只读项，不应触发事件"),
        tray::ID_OPEN_SERVICE_SETTINGS => log::warn!("本地服务设置尚未启用（阶段 B）"),
        tray::ID_QUIT => request_quit(app, QuitSource::Menu),
        // macOS 应用菜单的 Quit（⌘Q）：与托盘 Quit 同语义（§7.1），只是来源不同
        tray::ID_APP_QUIT => request_quit(app, QuitSource::Shortcut),
        other => log::warn!("未知菜单项: {other}"),
    }
}

/// 打开下载目录（§9.5 / A11）：不存在则创建；任何失败都恢复主窗口暴露错误，不静默失败
fn open_download_dir(app: &tauri::AppHandle) {
    use tauri_plugin_opener::OpenerExt;

    let dir = match app.path().download_dir() {
        Ok(dir) => dir,
        Err(e) => {
            log::warn!("无法定位下载目录: {e}");
            show_main_window(app);
            return;
        }
    };
    if let Err(e) = std::fs::create_dir_all(&dir) {
        log::warn!("创建下载目录失败 {}: {e}", dir.display());
        show_main_window(app);
        return;
    }
    if let Err(e) = app
        .opener()
        .open_path(dir.to_string_lossy().to_string(), None::<&str>)
    {
        log::warn!("打开下载目录失败 {}: {e}", dir.display());
        show_main_window(app);
    }
}

/// **仅 debug 构建**：从环境变量播种状态，便于验收需要「启动即处于某状态」的用例。
///
/// 阶段 A 没有真实传输，`TransferSnapshot` 恒为空，因此 A12（忙时退出二次确认）
/// 的确认分支根本无法到达。用环境变量播种可以在不污染 UI 与 IPC 面的前提下驱动它。
///
/// 用法：`QINGNIAO_DEBUG_TRANSFER="cancellable:committing:cleanup"`，例如 `1:0:0`
#[cfg(debug_assertions)]
fn seed_debug_state_from_env(app: &tauri::AppHandle) {
    let Ok(raw) = std::env::var("QINGNIAO_DEBUG_TRANSFER") else {
        return;
    };
    let nums: Vec<u32> = raw
        .split(':')
        .map(|s| s.trim().parse().unwrap_or(0))
        .collect();
    let snapshot = TransferSnapshot {
        accepting: true,
        cancellable_count: nums.first().copied().unwrap_or(0),
        committing_count: nums.get(1).copied().unwrap_or(0),
        cleanup_pending_count: nums.get(2).copied().unwrap_or(0),
        checkpointed: false,
    };
    log::warn!("已从环境变量播种传输快照: {snapshot:?}");
    app.state::<AppState>()
        .service
        .transfer
        .set_snapshot(snapshot);
}

/// 最终清理：由退出协议在 `Draining` 阶段调用（§7.2.2 第 3.4 / 3.5 步与第 4 步）
struct AppCleanup {
    app: tauri::AppHandle,
}

impl FinalCleanup for AppCleanup {
    fn stop_listener(&self) {
        // 阶段 A：由 fake 控制器停止；阶段 B 在此接入真实 HTTP listener 的停机与端口释放
        let state = self.app.state::<AppState>();
        state.service.service.stop();
        // 状态迁移后立即推送菜单文案（§5.2）
        state.sync_tray_status();
        log::info!("本地服务已停止: {:?}", state.service.service.snapshot());
    }

    fn remove_tray(&self) {
        let state = self.app.state::<AppState>();
        // 幂等：重复调用无副作用。先释放句柄，再移除系统图标
        if let Some(handles) = state.tray.lock().unwrap().take() {
            drop(handles);
        }
        self.app.remove_tray_by_id(tray::TRAY_ID);
    }

    fn exit(&self, code: i32) {
        self.app.exit(code);
    }
}

/// 退出确认：恢复主窗口后弹原生对话框（§7.2.2 第 2 步）。
///
/// 两个线程相关的要点：
/// 1. **不能同步等待**：`request_quit` 跑在菜单回调 / `RunEvent::ExitRequested` 的调用栈上
///    （即事件循环内），在此阻塞等待用户作答会死锁事件循环。故用 `show(callback)`。
/// 2. **回调不在主线程**：插件实现是
///    `run_on_main_thread(|| thread::spawn(|| block_on(dialog)))`，回调在工作线程上执行。
///    因此继续退出的动作必须用 `run_on_main_thread` 派回主线程——否则
///    `remove_tray()` 会跨线程调用 AppKit 的 `NSStatusBar::removeStatusItem`。
fn show_quit_dialog(app: &tauri::AppHandle, snapshot: TransferSnapshot) {
    use tauri_plugin_dialog::{DialogExt, MessageDialogButtons, MessageDialogKind};

    let pending = snapshot.cancellable_count + snapshot.committing_count;
    // 先把窗口亮出来，让用户能看到应用当前状态再决定
    show_main_window(app);
    log::warn!("退出确认：仍有 {pending} 个传输任务在进行，等待用户选择");

    let app_for_cb = app.clone();
    app.dialog()
        .message(format!(
            "仍有 {pending} 个传输任务正在进行。\n退出会中断它们（已完成的进度会保留）。"
        ))
        .title("退出青鸟")
        .kind(MessageDialogKind::Warning)
        .buttons(MessageDialogButtons::OkCancelCustom(
            "退出".to_string(),
            "取消".to_string(),
        ))
        .show(move |confirmed| {
            // **必须派回主线程**：插件的 `show` 是
            // `run_on_main_thread(|| thread::spawn(|| block_on(dialog)))`，
            // 也就是说这个回调在**工作线程**上执行；而继续退出的路径里
            // `cleanup.remove_tray()` 会走到 tray-icon 的 `remove()`，
            // 它直接调用 AppKit 的 `NSStatusBar::removeStatusItem`——AppKit 要求主线程。
            // 不派回去就是跨线程操作 AppKit（未定义行为）。
            let app = app_for_cb.clone();
            if let Err(e) = app_for_cb.run_on_main_thread(move || resume_quit(&app, confirmed)) {
                log::error!("无法把退出确认结果派回主线程: {e}");
            }
        });
}

/// 阶段二：把用户的选择送回退出协议（§7.2.2）
fn resume_quit(app: &tauri::AppHandle, confirmed: bool) {
    let state = app.state::<AppState>();
    let cleanup = AppCleanup { app: app.clone() };
    let step = state.exit.after_confirm(
        confirmed,
        state.service.service.as_ref(),
        state.service.transfer.as_ref(),
        state.service.transfer.as_ref(),
        &cleanup,
    );
    report_quit_step(app, step);
}

/// 统一退出入口：所有显式退出都必须走这里（§7.1）
pub fn request_quit(app: &tauri::AppHandle, source: QuitSource) {
    let state = app.state::<AppState>();
    let cleanup = AppCleanup { app: app.clone() };
    let step = state.exit.begin_quit(
        source,
        state.service.service.as_ref(),
        state.service.transfer.as_ref(),
        state.service.transfer.as_ref(),
        &cleanup,
    );
    report_quit_step(app, step);
}

/// 处理退出协议的每一步返回值
fn report_quit_step(app: &tauri::AppHandle, step: QuitStep) {
    match step {
        QuitStep::AlreadyInProgress | QuitStep::Cancelled => {}
        QuitStep::NeedsConfirm(snapshot) => show_quit_dialog(app, snapshot),
        QuitStep::Exited(report) => {
            if let Some(msg) = report.timeout_message() {
                log::warn!("退出提示: {msg}");
            }
        }
        QuitStep::AtomicCommitPending(report) => {
            if let Some(msg) = report.timeout_message() {
                log::warn!("退出提示: {msg}");
            }
            // 不可中断的原子提交尚未结束：把窗口亮出来告知用户，稍后可再次尝试退出
            show_main_window(app);
        }
    }
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    let app = tauri::Builder::default()
        // §8.1：single-instance **最先注册**；只有主实例继续 setup 副作用
        .plugin(tauri_plugin_single_instance::init(|app, _argv, _cwd| {
            show_main_window(app);
        }))
        .plugin(tauri_plugin_opener::init())
        // 退出二次确认用的原生对话框（非阻塞 show(callback)，见 show_quit_dialog）
        .plugin(tauri_plugin_dialog::init())
        // 应用菜单：macOS 必须自建，才能把系统预定义 Quit 换成走统一退出协议的项
        // （否则 ⌘Q 直接 terminate，绕过 §7.2 的 drain 与二次确认）。
        .menu(|app| {
            #[cfg(target_os = "macos")]
            {
                tray::build_app_menu(app)
            }
            #[cfg(not(target_os = "macos"))]
            {
                Ok(tauri::menu::Menu::default(app)?)
            }
        })
        // 菜单事件统一分派点：托盘菜单与窗口应用菜单都汇聚到这里
        .on_menu_event(|app, event| menu_action(app, event.id().as_ref()))
        .manage(AppState::new())
        .invoke_handler(tauri::generate_handler![
            load_config,
            save_config,
            send_webhook,
            upload_image,
            test_connection,
            log_event,
            // 仅 debug：A8 状态刷新的驱动入口
            #[cfg(debug_assertions)]
            debug_set_service_status
        ])
        .setup(|app| {
            // 先装 logger，之后的 tray / 生命周期日志才有处可去
            install_file_logger(app.handle());
            log::info!("青鸟启动：单实例判定通过，开始初始化常驻功能");

            // §8.1：仅在主实例、且 single-instance 判定之后构造**唯一**的 tray
            let handles = tray::build_tray(app.handle())?;
            app.state::<AppState>()
                .tray
                .lock()
                .unwrap()
                .replace(std::sync::Arc::new(handles));

            // 仅 debug：按环境变量播种状态（验收 A12 用）
            #[cfg(debug_assertions)]
            seed_debug_state_from_env(app.handle());
            Ok(())
        })
        .on_window_event(|window, event| {
            // §6 / D2：只拦截主窗口；prevent_close + hide，正常路径从不销毁重建。
            // 隐藏后还要让任务栏/Dock 图标一并消失（仿 cc-switch），
            // 应用退居纯托盘常驻；恢复窗口时在 show_main_window 里切回。
            if let tauri::WindowEvent::CloseRequested { api, .. } = event {
                if window.label() == "main" {
                    api.prevent_close();
                    let _ = window.hide();
                    #[cfg(target_os = "windows")]
                    let _ = window.set_skip_taskbar(true);
                    #[cfg(target_os = "macos")]
                    tray::apply_dock_policy(window.app_handle(), false);
                }
            }
        })
        .build(tauri::generate_context!())
        .expect("error while building tauri application");

    app.run(|app_handle, event| match event {
        // §5.3 / P1-2：Dock 单击经 Reopen 恢复窗口，不能只依赖「系统默认」
        #[cfg(target_os = "macos")]
        tauri::RunEvent::Reopen {
            has_visible_windows,
            ..
        } => {
            if !has_visible_windows {
                show_main_window(app_handle);
            }
        }
        // §7.2.1：仅 Exiting 放行，其余状态一律拦截。
        // AppHandle::exit() 自身会再次触发本事件，缺少放行分支将永远无法退出。
        //
        // 注意 macOS 的 ⌘Q / 应用菜单 Quit 走的也是这条事件——
        // 必须在 Running 时把它导入统一退出协议，否则会被自家的 prevent_exit()
        // 死死挡住，应用变得无法退出（§7.1 要求 ⌘Q 与菜单退出同语义）。
        tauri::RunEvent::ExitRequested { api, code, .. } => {
            let state = app_handle.state::<AppState>();
            log::info!(
                "ExitRequested: code={code:?} state={:?}",
                state.exit.state()
            );
            match state.exit.state() {
                // 已放行：真正退出
                ExitState::Exiting => {}
                // 已在退出流程中：继续拦截，等 drain 结束
                ExitState::Confirming | ExitState::Draining => api.prevent_exit(),
                // 全新的用户退出请求：先拦截，再走统一协议
                ExitState::Running => {
                    api.prevent_exit();
                    request_quit(app_handle, QuitSource::Shortcut);
                }
            }
        }
        _ => {}
    });
}

#[cfg(test)]
mod resident_tests {
    use super::*;
    use crate::service::ServiceFailure;
    use std::sync::{Arc, Mutex};

    /// 记录被推送的状态文案（替身）
    #[derive(Default)]
    struct RecordingDisplay {
        labels: Mutex<Vec<String>>,
    }

    impl tray::StatusDisplay for RecordingDisplay {
        fn show_status(&self, label: &str) {
            self.labels.lock().unwrap().push(label.to_string());
        }
    }

    /// A8：状态迁移必须把新文案推给状态项，且端口取自状态本身（不得硬编码）
    #[test]
    fn service_status_change_pushes_menu_label() {
        let state = AppState::new();
        let rec = Arc::new(RecordingDisplay::default());
        *state.tray.lock().unwrap() = Some(rec.clone());

        state.set_service_status(LocalServiceStatus::Running { bound_port: 12345 });
        state.set_service_status(LocalServiceStatus::Failed {
            kind: ServiceFailure::PortInUse,
            requested_port: 12345,
        });
        state.set_service_status(LocalServiceStatus::Stopped);

        assert_eq!(
            *rec.labels.lock().unwrap(),
            vec![
                "本地服务：运行中 · 127.0.0.1:12345".to_string(),
                "本地服务：启动失败（端口 12345 被占用）".to_string(),
                "本地服务：已停止".to_string(),
            ]
        );
    }

    /// 无 tray 时刷新不得 panic：启动早期与退出清理后都会出现该状态
    #[test]
    fn sync_without_tray_is_noop() {
        let state = AppState::new();
        state.sync_tray_status();
        state.set_service_status(LocalServiceStatus::Stopped);
    }
}
