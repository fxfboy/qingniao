// 青鸟 · 飞书消息助手 — Tauri 后端
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::io::Write;
use qingniao_core::keyring_store;
use tauri::Manager;

/// 常驻功能的模块划分（设计文档 dock/tray 常驻功能设计文档 v0.4）
pub mod lifecycle;
pub mod service;
pub mod tray;
/// 跨网文件传输的 **APP 侧适配层**（实现已迁入 core；见 transfer/mod.rs）

pub mod transfer;
/// 本地 HTTP 服务（127.0.0.1 双路由，D14/D20）
pub mod local_server;
use lifecycle::{ExitCoordinator, ExitState, FinalCleanup, QuitSource, QuitStep};
use service::{
    LocalServiceStatus, PhaseAService, ServiceStatusProvider, TransferActivityProvider,
    TransferController, TransferSnapshot, DEFAULT_LOCAL_PORT,
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

/// 单项权限探测结果（key ∈ "upload" | "drive"）
#[derive(Serialize)]
struct PermResult {
    key: String,
    ok: bool,
    apply_url: Option<String>,
}

/// 连接测试结果
#[derive(Serialize)]
struct ConnTestResult {
    app_name: String,
    permissions: Vec<PermResult>,
}

/// 1x1 透明 PNG，用于探测 im:resource:upload 权限（极小，不产生实际影响）
const PROBE_PNG_BASE64: &str = "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mNk+M9QDwADhgGAWjR9awAAAABJRU5ErkJggg==";

/// 从飞书权限不足的错误消息中提取申请链接，提取不到则按格式构造（scope 为对应权限名，如 im:resource:upload）
fn extract_apply_url(msg: &str, app_id: &str, scope: &str) -> String {
    if let Some(start) = msg.find("https://open.feishu.cn/app/") {
        let rest = &msg[start..];
        let end = rest
            .find(|c: char| c.is_whitespace() || c == '"' || c == '）' || c == ')' || c == '，')
            .unwrap_or(rest.len());
        return rest[..end].to_string();
    }
    format!(
        "https://open.feishu.cn/app/{}/auth?q={scope}&op_from=openapi&token_type=tenant",
        app_id
    )
}

/// 一个已保存的 webhook 机器人
#[derive(Serialize, Deserialize, Default, Clone)]
pub struct WebhookItem {
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub url: String,
    #[serde(default)]
    pub secret: String,
    /// schema v2 起的稳定 id（CLI 写入）；flatten 透传保留，APP 保存不丢失
    #[serde(flatten)]
    pub extra: serde_json::Map<String, serde_json::Value>,
}

/// 一条发送历史
#[derive(Serialize, Deserialize, Default, Clone)]
pub struct HistoryItem {
    #[serde(default)]
    pub time: String,
    // v2 前端记录使用 kind 字段（进 extra）；msg_type 为旧字段，缺失时容忍
    #[serde(default)]
    pub msg_type: String,
    #[serde(default)]
    pub summary: String,
    #[serde(default)]
    pub payload: serde_json::Value,
    #[serde(default)]
    pub ok: bool,
    #[serde(default)]
    pub status: String,
    /// v2 前端的 kind/dir/state/media/text 等展示字段原样透传，保存不丢失
    #[serde(flatten)]
    pub extra: serde_json::Map<String, serde_json::Value>,
}

/// 文件传输相关配置（设计文档 §10.2 / D5）
#[derive(Serialize, Deserialize, Default, Clone)]
pub struct TransferConfig {
    /// 配置端口（默认 9876）；实际绑定端口见服务状态
    #[serde(default)]
    pub configured_port: Option<u16>,
    /// 下载目录（None = 系统下载目录）
    #[serde(default)]
    pub download_dir: Option<String>,
}

/// 应用配置
#[derive(Serialize, Deserialize, Default, Clone)]
pub struct AppConfig {
    #[serde(default)]
    pub webhooks: Vec<WebhookItem>,
    #[serde(default)]
    pub history: Vec<HistoryItem>,
    #[serde(default)]
    pub last_webhook: Option<usize>,
    #[serde(default)]
    pub last_type: Option<String>,
    #[serde(default)]
    pub app_id: String,
    #[serde(default)]
    pub app_secret: String,
    /// 配置 schema 版本（现文件无版本=1）
    #[serde(default)]
    pub schema_version: u32,
    #[serde(default)]
    pub transfer: TransferConfig,
    /// 偏好（主题等，宽松结构）
    #[serde(default)]
    pub hotkey: Option<String>,
    #[serde(default)]
    pub prefs: Option<serde_json::Value>,
    /// last_bot_id（CLI 写入）等未知字段透传，保存不丢失（方案 v3 D5）
    #[serde(flatten)]
    pub extra: serde_json::Map<String, serde_json::Value>,
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

/// 把 dir 的前缀替换成短标签（HOME → `~`、APPDATA → `%APPDATA%`）；前缀不匹配则原样返回
fn abbrev_path(dir: &Path, prefix: Option<&Path>, label: &str) -> String {
    if let Some(p) = prefix {
        if let Ok(rel) = dir.strip_prefix(p) {
            return if rel.as_os_str().is_empty() {
                label.to_string()
            } else {
                PathBuf::from(label).join(rel).display().to_string()
            };
        }
    }
    dir.display().to_string()
}

/// 数据目录展示串：按平台缩写（macOS/Linux 用 `~`，Windows 用 `%APPDATA%`），
/// 与「关于」面板的既有写法一致；前缀不匹配时退回真实绝对路径。
fn data_dir_display(dir: &Path) -> String {
    #[cfg(target_os = "windows")]
    let (prefix, label) = (std::env::var_os("APPDATA").map(PathBuf::from), "%APPDATA%");
    #[cfg(not(target_os = "windows"))]
    let (prefix, label) = (std::env::var_os("HOME").map(PathBuf::from), "~");
    abbrev_path(dir, prefix.as_deref(), label)
}

/// 「关于」页「最近更新」= 这份产物的打包时刻，取 build.rs 编译期写入的绝对时刻
/// （`QINGNIAO_BUILD_EPOCH`），再按运行机器的本地时区格式化成 `YYYY-MM-DD HH:MM`。
///
/// 刻意不用版本号推日期：同一个版本可能被多次打包，展示的必须是**这一份产物**
/// 什么时候打出来的，重新打包时间就要跟着走。
fn build_time_label() -> String {
    let Some(raw) = option_env!("QINGNIAO_BUILD_EPOCH") else {
        return String::new();
    };
    let Ok(epoch) = raw.parse::<i64>() else {
        return String::new();
    };
    if epoch <= 0 {
        return String::new();
    }
    let Ok(utc) = time::OffsetDateTime::from_unix_timestamp(epoch) else {
        return String::new();
    };
    let at = time::UtcOffset::current_local_offset()
        .map(|off| utc.to_offset(off))
        .unwrap_or(utc);
    format!(
        "{:04}-{:02}-{:02} {:02}:{:02}",
        at.year(),
        at.month() as u8,
        at.day(),
        at.hour(),
        at.minute()
    )
}

/// 「关于」面板的动态元信息：数据目录（取 APP 真实使用的 app_config_dir，各平台不同）
/// 与最近更新的打包时间；前端不再硬编码路径与日期
#[tauri::command]
fn about_info(app: tauri::AppHandle) -> Result<serde_json::Value, String> {
    let dir = app
        .path()
        .app_config_dir()
        .map_err(|e| format!("无法定位数据目录: {e}"))?;
    Ok(serde_json::json!({
        "data_dir": data_dir_display(&dir),
        "updated_at": build_time_label(),
    }))
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
fn save_config(app: tauri::AppHandle, config: serde_json::Value) -> Result<(), String> {
    let path = config_path(&app)?;
    // R8（M0b）：锁内「整体替换 raw + history 以磁盘为准」；文件锁/原子写/schema_version 由 core 处理。
    // 前端快照不再拥有 history 的落盘权（D7 推完：删除走 delete_history_item、追加走 append_history_item）。
    qingniao_core::config::replace_preserving_history(&path, config).map_err(|e| {
        write_log(&app, LV_ERROR, &format!("save_config 失败: {e}"));
        e
    })?;
    write_log(&app, LV_INFO, "save_config: 已保存（history 以磁盘为准保留）");
    Ok(())
}

/// R8（M0b）：删除单条历史（锁内按 time+kind 定位；前端只渲染不落盘）
#[tauri::command]
fn delete_history_item(app: tauri::AppHandle, time: String, kind: String) -> Result<usize, String> {
    let path = config_path(&app)?;
    let n = qingniao_core::config::delete_history_entry(&path, &time, &kind).map_err(|e| {
        write_log(&app, LV_ERROR, &format!("delete_history_item 失败: {e}"));
        e
    })?;
    write_log(&app, LV_INFO, &format!("delete_history_item: 删除 {n} 条"));
    Ok(n)
}

/// R8（M0b）：追加单条历史（kind=file 传输记录的落盘路径；HISTORY_CAP 由 core 强制）
#[tauri::command]
fn append_history_item(app: tauri::AppHandle, rec: serde_json::Value) -> Result<(), String> {
    let path = config_path(&app)?;
    qingniao_core::config::append_history_item(&path, rec).map_err(|e| {
        write_log(&app, LV_ERROR, &format!("append_history_item 失败: {e}"));
        e
    })
}

/// 读取本地图片文件并返回 base64（拖拽图片 → 输入框 chip 用）
/// 仅允许常见图片扩展名，读取上限 10 MB
#[tauri::command]
fn read_file_base64(app: tauri::AppHandle, path: String) -> Result<String, String> {
    const MAX: u64 = 10 * 1024 * 1024;
    let p = std::path::Path::new(&path);
    let ext_ok = p.extension()
        .and_then(|e| e.to_str())
        .map(|e| matches!(e.to_ascii_lowercase().as_str(), "png" | "jpg" | "jpeg" | "gif" | "webp" | "bmp" | "svg"))
        .unwrap_or(false);
    if !ext_ok {
        return Err("仅支持读取图片文件".into());
    }
    let meta = std::fs::metadata(p).map_err(|e| format!("无法读取文件信息: {e}"))?;
    if meta.len() > MAX {
        return Err("图片超过 10 MB，请压缩后再试".into());
    }
    let bytes = std::fs::read(p).map_err(|e| format!("读取图片失败: {e}"))?;
    write_log(&app, LV_DEBUG, &format!("read_file_base64: {} ({} B)", p.display(), bytes.len()));
    use base64::Engine as _;
    Ok(base64::engine::general_purpose::STANDARD.encode(bytes))
}

/* ===================== 传输密钥（keyring，D15/D19/D21） ===================== */

#[derive(Serialize)]
struct KeyStatus {
    has_key: bool,
    kid: String,
    fingerprint: String,
    has_previous: bool,
}

/// 当前密钥状态（指纹/kid = SHA-256(K) 前 8 字节 hex，D19）
#[tauri::command]
fn key_status() -> Result<KeyStatus, String> {
    let cur = keyring_store::get_current()?;
    let prev = keyring_store::get_previous()?;
    match cur {
        Some(hex_key) => {
            let key = transfer::crypto::key_from_hex(&hex_key)?;
            Ok(KeyStatus {
                kid: transfer::crypto::fingerprint(&key),
                fingerprint: transfer::crypto::fingerprint(&key),
                has_key: true,
                has_previous: prev.is_some(),
            })
        }
        None => Ok(KeyStatus {
            has_key: false,
            kid: String::new(),
            fingerprint: String::new(),
            has_previous: false,
        }),
    }
}

/// 生成本机主密钥并存入凭据库（覆盖旧密钥；配对页「生成」入口）
#[tauri::command]
fn key_generate(app: tauri::AppHandle) -> Result<KeyStatus, String> {
    let hex_key = keyring_store::generate();
    keyring_store::set_current(&hex_key)?;
    write_log(&app, LV_INFO, "key_generate: 已生成新主密钥并存入凭据库");
    key_status()
}

/// 导出主密钥明文（hex 64 字符）——仅用于两台机器配对，严禁经飞书传输（§12.3）
#[tauri::command]
fn key_export() -> Result<String, String> {
    keyring_store::get_current()?.ok_or_else(|| "尚未配置主密钥".to_string())
}

/// 导入主密钥（hex 64 字符，覆盖本机；导入前前端已展示指纹供人工核对）
#[tauri::command]
fn key_import(hex: String) -> Result<KeyStatus, String> {
    let key = transfer::crypto::key_from_hex(&hex)?;
    keyring_store::import(&transfer::crypto::hex(&key))?;
    key_status()
}

/// 轮换：生成新密钥，旧密钥降级为 previous 保留 30 天（§12.3）
#[tauri::command]
fn key_rotate() -> Result<KeyStatus, String> {
    if keyring_store::get_current()?.is_none() {
        return Err("尚未配置主密钥，无需轮换".into());
    }
    let hex_key = keyring_store::generate();
    keyring_store::rotate(&hex_key)?;
    key_status()
}

/// 引擎的传输活动适配器：实现 TransferActivityProvider / TransferController，
/// 让退出协议（checkpoint / cancel_all / join_to_safe_point）作用于真实传输任务。
struct EngineTransferAdapter(std::sync::Arc<transfer::engine::Engine>);

impl TransferActivityProvider for EngineTransferAdapter {
    fn snapshot(&self) -> TransferSnapshot {
        let cleanup_pending = std::fs::read_to_string(self.0.work_dir().join("pending_deletes.json"))
            .ok()
            .and_then(|s| serde_json::from_str::<Vec<String>>(&s).ok())
            .map(|v| v.len() as u32)
            .unwrap_or(0);
        TransferSnapshot {
            accepting: true,
            cancellable_count: self.0.cancellable_count() as u32,
            committing_count: 0, // commit（rename/删云端）为不可中断临界区，不出现跨取消窗口
            cleanup_pending_count: cleanup_pending,
            checkpointed: true, // 断点信息按片实时持久化，无需显式 checkpoint
        }
    }
}

impl TransferController for EngineTransferAdapter {
    fn checkpoint(&self) {
        // 断点信息（.part + 侧车 JSON）按片实时落盘，无需额外动作
    }
    fn cancel_all(&self) {
        self.0.cancel_all();
    }
    fn join_to_safe_point(&self, timeout: std::time::Duration) -> bool {
        let deadline = std::time::Instant::now() + timeout;
        while self.0.cancellable_count() > 0 {
            if std::time::Instant::now() >= deadline {
                return false;
            }
            std::thread::sleep(std::time::Duration::from_millis(200));
        }
        true
    }
}

/// 读取应用配置（失败返回默认值）
fn read_app_config(app: &tauri::AppHandle) -> AppConfig {
    config_path(app)
        .ok()
        .and_then(|p| std::fs::read_to_string(p).ok())
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

/* ===================== 文件传输命令（P2 上传 / P3 下载与服务） ===================== */

/// 服务实际绑定端口（仅 Running 时有值）。
///
/// 注：M0a 期间仍用它生成取回链接（保持行为零变更）；**M0c 将改为 `configured_port`**
/// 并把本函数降级为「本机作为接收端的可达性」描述（协议 v1.3 D23）。
pub fn service_bound_port(app: &tauri::AppHandle) -> Option<u16> {
    let state = app.state::<AppState>();
    let snap = state.service.read().unwrap().status.snapshot();
    match snap {
        LocalServiceStatus::Running { bound_port } => Some(bound_port),
        _ => None,
    }
}

/// 上传任务发起（§8）：校验后立即返回 task_id，后台线程执行；进度走 transfer://progress
#[tauri::command]
fn transfer_start_upload(
    app: tauri::AppHandle,
    path: String,
    webhook_url: String,
    webhook_secret: Option<String>,
) -> Result<serde_json::Value, String> {
    let state = app.state::<AppState>();
    let engine = state.engine.get().ok_or("传输引擎未初始化")?;
    let cfg = read_app_config(&app);
    let accepted = engine.start_upload(
        transfer::engine::UploadRequest {
            path,
            webhook_url,
            webhook_secret: webhook_secret.unwrap_or_default(),
        },
        cfg.app_id,
        cfg.app_secret,
    )?;
    write_log(&app, LV_INFO, &format!("transfer_start_upload: task_id={} size={}", accepted.task_id, accepted.size));
    Ok(serde_json::json!({
        "task_id": accepted.task_id,
        "name": accepted.name,
        "size": accepted.size,
    }))
}

/// 取消传输任务（片边界生效）
#[tauri::command]
fn transfer_cancel(app: tauri::AppHandle, task_id: String) -> Result<(), String> {
    let state = app.state::<AppState>();
    let engine = state.engine.get().ok_or("传输引擎未初始化")?;
    engine.cancel(&task_id)
}

/// 解析取回链接/裸 payload（§9.2 兜底入口；创建会话，不产生任何下载副作用）
#[tauri::command]
fn transfer_parse_payload(app: tauri::AppHandle, input: String) -> Result<serde_json::Value, String> {
    let state = app.state::<AppState>();
    let engine = state.engine.get().ok_or("传输引擎未初始化")?;
    let ev = match engine.evaluate_payload(&input) {
        Ok(ev) => ev,
        Err(e) => {
            // 失败也要留痕：兜底入口此前既无 toast 也无日志，出问题只能靠猜
            write_log(&app, LV_WARN, &format!("transfer_parse_payload: 解析失败（输入 {} 字节）: {e}", input.len()));
            return Err(e);
        }
    };
    let fingerprint = ev.fingerprint.clone();
    let sess = engine.create_session(&input, ev).map_err(|e| {
        write_log(&app, LV_WARN, &format!("transfer_parse_payload: 创建会话失败: {e}"));
        e
    })?;
    write_log(
        &app,
        LV_INFO,
        &format!("transfer_parse_payload: 会话已创建 name={} size={} fingerprint={}", sess.name, sess.size, &fingerprint[..8.min(fingerprint.len())]),
    );
    Ok(serde_json::json!({
        "session_id": sess.handle,
        "name": sess.name,
        "size": sess.size,
        // 回传指纹：前端据此与 transfer://downloaded 事件去重（同一文件不会落两条记录）
        "fingerprint": fingerprint,
        // 取回页展示所需：发送时间 / 剩余有效期倒计时 / 保存目录
        // （与会话同源，保证应用内取回页与 /dl 确认页显示的时长一致）
        "created_at": sess.created_at,
        "expires_at": sess.expires_at,
        "download_dir": sess.download_dir,
    }))
}

/// 确认下载（与 /dl/confirm 同一引擎入口）：后台线程执行，进度走 transfer://progress
#[tauri::command]
fn transfer_confirm_download(app: tauri::AppHandle, session_id: String) -> Result<serde_json::Value, String> {
    let state = app.state::<AppState>();
    let engine = state.engine.get().cloned().ok_or("传输引擎未初始化")?;
    let handle = session_id;
    // 会话信息要回传 task_id：先在当前线程 claim（单消费），再放线程执行
    let claim = engine.claim_session(&handle);
    match claim {
        Ok(transfer::engine::ClaimResult::AlreadyDone { path }) => {
            // 幂等：直接返回完成结果
            Ok(serde_json::json!({
                "task_id": handle,
                "already_done": true,
                "final_path": path,
            }))
        }
        Ok(transfer::engine::ClaimResult::Start(pending)) => {
            let task_id = pending.task_id.clone().ok_or("内部错误：任务标识缺失")?;
            let engine2 = engine.clone();
            let app2 = app.clone();
            let task_id2 = task_id.clone();
            std::thread::Builder::new()
                .name(format!("qn-download-{task_id}"))
                .spawn(move || {
                    if let Err(msg) = engine2.run_download_sync(&pending) {
                        log::warn!("下载任务 {task_id2} 失败: {msg}");
                    }
                    let _ = &app2;
                })
                .map_err(|e| format!("启动下载线程失败: {e}"))?;
            Ok(serde_json::json!({ "task_id": task_id }))
        }
        Err(transfer::engine::ClaimError::Busy) => Err("正在下载：同一文件同时只会跑一个任务".into()),
        Err(transfer::engine::ClaimError::Expired) => Err("确认已过期，请重新解析链接".into()),
        Err(transfer::engine::ClaimError::NotFound) => Err("会话不存在或已使用，请重新解析链接".into()),
    }
}

/// 轮询任务终态（前端注册监听前事件可能已发出的补偿）
#[tauri::command]
fn transfer_task_state(app: tauri::AppHandle, task_id: String) -> Result<Option<serde_json::Value>, String> {
    let state = app.state::<AppState>();
    let engine = state.engine.get().ok_or("传输引擎未初始化")?;
    Ok(engine.take_final_event(&task_id))
}

/// 本地服务状态（§10.2）
#[tauri::command]
fn transfer_service_status(app: tauri::AppHandle) -> Result<serde_json::Value, String> {
    let state = app.state::<AppState>();
    let snap = state.service.read().unwrap().status.snapshot();
    Ok(match snap {
        LocalServiceStatus::Running { bound_port } => serde_json::json!({"status": "Running", "bound_port": bound_port}),
        LocalServiceStatus::Failed { requested_port, .. } => serde_json::json!({"status": "Failed", "requested_port": requested_port}),
        LocalServiceStatus::Starting => serde_json::json!({"status": "Starting"}),
        LocalServiceStatus::Stopping => serde_json::json!({"status": "Stopping"}),
        LocalServiceStatus::Stopped => serde_json::json!({"status": "Stopped"}),
    })
}

/// 保存并重启本地服务（§10.2：configured_port 变更；端口占用不漂移）
#[tauri::command]
fn transfer_service_restart(app: tauri::AppHandle, port: u16) -> Result<serde_json::Value, String> {
    let state = app.state::<AppState>();
    let real = state.real_service.get().ok_or("本地服务未初始化")?;
    let page = include_str!("../assets/transfer-confirm.html");
    let st = local_server::restart_service(real, port, page);
    state.set_service_status(st.clone());
    Ok(match st {
        LocalServiceStatus::Running { bound_port } => serde_json::json!({"status": "Running", "bound_port": bound_port}),
        LocalServiceStatus::Failed { requested_port, .. } => serde_json::json!({"status": "Failed", "requested_port": requested_port}),
        _ => serde_json::json!({"status": "Stopped"}),
    })
}

/// 打开下载目录（filecard「打开目录」/ 托盘菜单共用）
#[tauri::command]
fn open_download_dir(app: tauri::AppHandle) -> Result<(), String> {
    use transfer::engine::Host as _;
    let dir = transfer::TauriHost::new(app.clone()).resolve_download_dir()?;
    use tauri_plugin_opener::OpenerExt;
    app.opener()
        .open_path(dir, None::<&str>)
        .map_err(|e| format!("打开目录失败: {e}"))
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

// ===== Agent-CLI M3：消息组装/识别/发送走 qingniao-core（方案 v3 §九）=====
use qingniao_core::config as core_config;

/// 类型识别（前端 analyze；序号 + 150ms 防抖由前端负责，D15）
pub fn analyze_impl(text: &str, chips: usize) -> serde_json::Value {
    let d = qingniao_core::message::detect_type(text, chips);
    serde_json::json!({ "t": d.t.as_str(), "why": d.why })
}

#[tauri::command]
fn analyze(text: String, chips: usize) -> serde_json::Value {
    analyze_impl(&text, chips)
}

#[derive(Serialize)]
pub struct SendMessageOutcome {
    pub ok: bool,
    pub status_line: String,
    pub state: String,
    pub http_status: Option<u16>,
    pub feishu_code: Option<i64>,
    pub feishu_msg: Option<String>,
    pub body_summary: Option<String>,
    pub error: Option<String>,
    pub record: serde_json::Value,
}

fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// 发送 + 历史写入（D7：APP 消息历史的唯一写入口）。
/// APP 维持任意 http(s) 兼容（D10），allow_insecure = true。
/// 网络/超时失败也写入历史（state=failed/unknown）；Err 仅用于发送前的 usage/config 失败。
pub fn send_message_impl(
    cfg_path: &Path,
    bot_key: Option<String>,
    msg_type: String,
    text: String,
    image_keys: Vec<String>,
    title: Option<String>,
    summary: Option<String>,
    extra: Option<serde_json::Value>,
    now_secs: u64,
) -> Result<SendMessageOutcome, String> {
    use qingniao_core::message::{build_payload, detect_type, MsgType};
    use qingniao_core::send::{dispatch_send, record_send};

    let loaded = core_config::load(cfg_path)?;
    for w in &loaded.warnings {
        log::warn!("send_message: {w}");
    }

    let mt = if msg_type == "auto" {
        detect_type(&text, image_keys.len()).t
    } else {
        MsgType::from_wire(&msg_type).ok_or_else(|| format!("未知消息类型: {msg_type}"))?
    };
    let key_refs: Vec<&str> = image_keys.iter().map(String::as_str).collect();
    let payload = build_payload(&text, mt, &key_refs, title.as_deref())?;

    let recorded =
        dispatch_send(&loaded.config, bot_key.as_deref(), &payload, true, now_secs)
        .map_err(|f| f.message)?;

    let mut rec = recorded.record.clone();
    if let Some(s) = summary {
        rec["summary"] = serde_json::Value::String(s);
    }
    if let Some(serde_json::Value::Object(map)) = extra {
        for (k, v) in map {
            rec[k] = v;
        }
    }
    if let Err(e) = record_send(cfg_path, rec.clone()) {
        log::warn!("send_message: 历史写入失败（发送已完成）: {e}");
    }

    Ok(match &recorded.result {
        Ok(r) => SendMessageOutcome {
            ok: r.ok,
            status_line: recorded.status_line,
            state: recorded.state.into(),
            http_status: r.http_status,
            feishu_code: r.feishu_code,
            feishu_msg: r.feishu_msg.clone(),
            body_summary: r.body_summary.clone(),
            error: None,
            record: rec,
        },
        Err(f) => SendMessageOutcome {
            ok: false,
            status_line: f.message.clone(),
            state: recorded.state.into(),
            http_status: None,
            feishu_code: None,
            feishu_msg: None,
            body_summary: None,
            error: Some(f.message.clone()),
            record: rec,
        },
    })
}

#[tauri::command]
#[allow(clippy::too_many_arguments)]
fn send_message(
    app: tauri::AppHandle,
    bot_key: Option<String>,
    msg_type: String,
    text: String,
    image_keys: Vec<String>,
    title: Option<String>,
    summary: Option<String>,
    extra: Option<serde_json::Value>,
) -> Result<SendMessageOutcome, String> {
    let cfg_path = config_path(&app)?;
    send_message_impl(&cfg_path, bot_key, msg_type, text, image_keys, title, summary, extra, now_secs())
}

/// 重新发送已存 payload（历史「重发」按钮）：按 time 更新原记录，不追加新条目（与现前端行为一致）
pub fn resend_payload_impl(
    cfg_path: &Path,
    payload: serde_json::Value,
    bot_key: Option<String>,
    rec_time: String,
    now_secs: u64,
) -> Result<SendMessageOutcome, String> {
    use qingniao_core::send::{send_prebuilt, SendErrorKind};

    let loaded = core_config::load(cfg_path)?;
    let attempt = send_prebuilt(&loaded.config, bot_key.as_deref(), &payload, true, now_secs)
        .map_err(|f| f.message)?;
    let (ok, state, status_line) = match &attempt.result {
        Ok(r) => (
            r.ok,
            if r.ok { "sent" } else { "failed" },
            qingniao_core::send::status_line_of(r),
        ),
        Err(f) => (
            false,
            if f.kind == SendErrorKind::Timeout { "unknown" } else { "failed" },
            f.message.clone(),
        ),
    };
    core_config::modify(cfg_path, |cfg| {
        if let Some(arr) = cfg.raw.get_mut("history").and_then(|v| v.as_array_mut()) {
            for rec in arr.iter_mut() {
                if rec.get("time").and_then(|v| v.as_str()) == Some(rec_time.as_str()) {
                    rec["ok"] = serde_json::json!(ok);
                    rec["status"] = serde_json::json!(status_line);
                    rec["state"] = serde_json::json!(state);
                    break;
                }
            }
        }
        Ok(())
    })?;
    let (http_status, feishu_code, feishu_msg, body_summary) = match &attempt.result {
        Ok(r) => (r.http_status, r.feishu_code, r.feishu_msg.clone(), r.body_summary.clone()),
        Err(_) => (None, None, None, None),
    };
    Ok(SendMessageOutcome {
        ok,
        status_line,
        state: state.into(),
        http_status,
        feishu_code,
        feishu_msg,
        body_summary,
        error: None,
        record: serde_json::Value::Null,
    })
}

#[tauri::command]
fn resend_payload(
    app: tauri::AppHandle,
    payload: serde_json::Value,
    bot_key: Option<String>,
    rec_time: String,
) -> Result<SendMessageOutcome, String> {
    let cfg_path = config_path(&app)?;
    resend_payload_impl(&cfg_path, payload, bot_key, rec_time, now_secs())
}

// ===== Agent-CLI M4：技能 / CLI 安装（完全模仿 paseo，方案 v3 §七 D13）=====

/// bundle 技能源目录：按 资源目录 → 可执行文件同目录 → 仓库 skills/ 依次探测，
/// 取第一个真实包含 `skills/qingniao/` 的候选；都不存在返回 None（不再盲回退到编译期路径）。
///
/// 三处候选各有用处：
/// - 资源目录：macOS DMG 等打包安装后 `resources/skills`；
/// - exe 同级：Windows 便携 zip（CI 用 `--no-bundle`，resources 不落盘，`skills/` 直接放在 exe 旁，
///   且 Windows 上 `resource_dir()` 就等于 exe 目录）；
/// - 仓库：开发期 `cargo tauri dev`，`CARGO_MANIFEST_DIR` 是编译期常量，仅当路径真的存在才采用。
fn bundled_skills_dir(app: &tauri::AppHandle) -> Option<PathBuf> {
    let mut candidates: Vec<PathBuf> = Vec::new();
    if let Ok(rd) = app.path().resource_dir() {
        candidates.push(rd.join("skills"));
    }
    if let Some(dir) = std::env::current_exe().ok().and_then(|e| e.parent().map(PathBuf::from)) {
        candidates.push(dir.join("skills"));
    }
    candidates.push(PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("..").join("skills"));
    pick_skills_dir(candidates)
}

/// 取第一个真实包含 `skills/<SKILL_NAME>/` 的候选目录
fn pick_skills_dir(candidates: impl IntoIterator<Item = PathBuf>) -> Option<PathBuf> {
    candidates
        .into_iter()
        .find(|p| p.join(qingniao_core::skills::SKILL_NAME).is_dir())
}

fn skill_targets() -> Vec<qingniao_core::skills::SkillTarget> {
    let home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("USERPROFILE").map(PathBuf::from))
        .unwrap_or_else(|| PathBuf::from("/"));
    qingniao_core::skills::default_targets(&home)
}

/// 技能源缺失时的统一提示（Windows 便携包 / 精简分发）
const ERR_SKILLS_SOURCE_MISSING: &str =
    "当前安装包未内置技能源（skills/）。请改用 npm 安装路径：npx skills add fxfboy/qingniao";

#[tauri::command]
fn skills_status(app: tauri::AppHandle) -> Result<qingniao_core::skills::SkillsStatus, String> {
    let targets = skill_targets();
    match bundled_skills_dir(&app) {
        Some(dir) => qingniao_core::skills::get_status(&dir, &targets),
        None => Ok(qingniao_core::skills::missing_source_status(&targets)),
    }
}

#[tauri::command]
fn install_skills(app: tauri::AppHandle) -> Result<qingniao_core::skills::SkillsStatus, String> {
    let dir = bundled_skills_dir(&app).ok_or(ERR_SKILLS_SOURCE_MISSING)?;
    let targets = skill_targets();
    qingniao_core::skills::install(&dir, &targets)?;
    qingniao_core::skills::get_status(&dir, &targets)
}

#[tauri::command]
fn uninstall_skills(app: tauri::AppHandle) -> Result<qingniao_core::skills::SkillsStatus, String> {
    let targets = skill_targets();
    qingniao_core::skills::uninstall(&targets)?;
    match bundled_skills_dir(&app) {
        Some(dir) => qingniao_core::skills::get_status(&dir, &targets),
        None => Ok(qingniao_core::skills::missing_source_status(&targets)),
    }
}

/// CLI 二进制来源：打包环境 resources/bin；开发环境 current_exe 同目录（workspace target）
fn cli_source_path(app: &tauri::AppHandle) -> Option<PathBuf> {
    let exe_name = if cfg!(target_os = "windows") { "qingniao-cli.exe" } else { "qingniao-cli" };
    if let Ok(rd) = app.path().resource_dir() {
        let p = rd.join("bin").join(exe_name);
        if p.exists() {
            return Some(p);
        }
    }
    let exe = std::env::current_exe().ok()?;
    let p = exe.parent()?.join(exe_name);
    p.exists().then_some(p)
}

fn cli_install_target() -> Option<PathBuf> {
    #[cfg(target_os = "windows")]
    {
        std::env::var_os("LOCALAPPDATA")
            .map(|d| PathBuf::from(d).join("qingniao").join("bin").join("qingniao.exe"))
    }
    #[cfg(not(target_os = "windows"))]
    {
        std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".local").join("bin").join("qingniao"))
    }
}

#[tauri::command]
fn cli_install_status(app: tauri::AppHandle) -> Result<serde_json::Value, String> {
    let target = cli_install_target();
    let target_dir = target.as_ref().and_then(|p| p.parent().map(PathBuf::from));
    let source_available = cli_source_path(&app).is_some();
    let installed = target.as_ref().map(|p| p.exists()).unwrap_or(false);
    Ok(serde_json::json!({
        "installed": installed,
        "source_available": source_available,
        "target_path": target.map(|p| p.display().to_string()),
        // PATH 要加的是目录而不是可执行文件本身
        "target_dir": target_dir.map(|p| p.display().to_string()),
        // Windows 不像 macOS/Linux 那样能改 shell rc，安装目录需用户手动加进 PATH（方案 §七）
        "manual_path_setup": cfg!(target_os = "windows"),
    }))
}

#[tauri::command]
fn install_cli(app: tauri::AppHandle) -> Result<serde_json::Value, String> {
    let source = cli_source_path(&app).ok_or(
        "未找到 CLI 二进制（开发模式请先 cargo build -p qingniao-cli；打包环境由 CI 嵌入 resources）",
    )?;
    let target = cli_install_target().ok_or("无法定位安装目录（HOME/LOCALAPPDATA）")?;
    qingniao_core::skills::install_cli_binary(&source, &target)?;
    // unix：幂等往 shell rc 追加 PATH（仅 bash/zsh）
    let mut shell_updated = false;
    #[cfg(not(target_os = "windows"))]
    {
        if let Some(home) = std::env::var_os("HOME").map(PathBuf::from) {
            let shell = std::env::var("SHELL").unwrap_or_default();
            let bin_dir = target
                .parent()
                .map(|p| p.to_path_buf())
                .unwrap_or_else(|| home.join(".local").join("bin"));
            shell_updated = qingniao_core::skills::ensure_path_in_shell_rc(&home, &shell, &bin_dir)?;
        }
    }
    log::info!("install_cli: target={} shell_updated={}", target.display(), shell_updated);
    cli_install_status(app)
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
    let upload_perm;
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
            write_log(&app, LV_INFO, "test_connection: im:resource:upload 权限已开通");
            upload_perm = PermResult { key: "upload".to_string(), ok: true, apply_url: None };
        } else {
            let msg = probe_json["msg"].as_str().unwrap_or("");
            let apply_url = extract_apply_url(msg, &app_id, "im:resource:upload");
            write_log(&app, LV_WARN, &format!(
                "test_connection: 缺少 im:resource:upload 权限 code={} msg={}",
                probe_json["code"], probe_json["msg"]
            ));
            upload_perm = PermResult { key: "upload".to_string(), ok: false, apply_url: Some(apply_url) };
        }
    }

    // 4. 探测 drive:drive（云空间）权限：读取云空间根文件夹元信息（设计文档 §11 指定接口）。
    // 已知边界：root_folder/meta 用 drive:drive 的只读子集也能通过（存在假阳性可能），
    // 但申请链接固定请求完整的 drive:drive；真正的写操作失败会在传输任务里以 403 呈现。
    // tenant_access_token 与 bot:v3:info 维持隐式校验：失败即整体连接失败，属凭证错误，无申请链接语义。
    let drive_perm = {
        let drive_result = |ok: bool, apply_url: Option<String>| PermResult {
            key: "drive".to_string(),
            ok,
            apply_url,
        };
        match client
            .get("https://open.feishu.cn/open-apis/drive/explorer/v2/root_folder/meta")
            .bearer_auth(&token)
            .send()
        {
            Ok(resp) => match resp.json::<serde_json::Value>() {
                Ok(j) if j["code"].as_i64().unwrap_or(-1) == 0 => {
                    write_log(&app, LV_INFO, "test_connection: drive:drive 权限已开通");
                    drive_result(true, None)
                }
                Ok(j) => {
                    let msg = j["msg"].as_str().unwrap_or("");
                    let apply_url = extract_apply_url(msg, &app_id, "drive:drive");
                    write_log(&app, LV_WARN, &format!(
                        "test_connection: 缺少 drive:drive 权限 code={} msg={}",
                        j["code"], j["msg"]
                    ));
                    drive_result(false, Some(apply_url))
                }
                Err(e) => {
                    write_log(&app, LV_WARN, &format!("test_connection: drive 权限探测响应解析失败: {e}"));
                    drive_result(false, None)
                }
            },
            Err(e) => {
                write_log(&app, LV_WARN, &format!("test_connection: drive 权限探测请求失败: {}", err_chain(&e)));
                drive_result(false, None)
            }
        }
    };

    Ok(ConnTestResult {
        app_name,
        permissions: vec![upload_perm, drive_perm],
    })
}

// ---------------------------------------------------------------------------
// 常驻功能：应用状态、窗口恢复、菜单分派、退出协议、启动装配
// ---------------------------------------------------------------------------

/// 应用级共享状态（§5.2：Rust `AppState` 持有服务状态、传输快照与菜单项句柄）
pub struct AppState {
    pub exit: ExitCoordinator,
    /// 阶段 A 为 fake 集合；setup 完成阶段 B 装配后替换为真实现（RwLock 支持替换）
    pub service: std::sync::RwLock<PhaseAService>,
    /// 跨网文件传输引擎（setup 时初始化；传输功能属于 dock-tray 阶段 B 落地）
    pub engine: std::sync::OnceLock<std::sync::Arc<transfer::engine::Engine>>,
    /// 真实本地服务（setup 时初始化）
    pub real_service: std::sync::OnceLock<std::sync::Arc<local_server::RealLocalService>>,
    /// 状态项显示端。存为 trait object 以便单测注入替身（A8 的 `unit/fake` 验收）
    pub tray: std::sync::Mutex<Option<std::sync::Arc<dyn tray::StatusDisplay>>>,}

impl AppState {
    fn new() -> Self {
        Self {
            exit: ExitCoordinator::default(),
            service: std::sync::RwLock::new(PhaseAService::new()),
            engine: std::sync::OnceLock::new(),
            real_service: std::sync::OnceLock::new(),
            tray: std::sync::Mutex::new(None),
        }
    }

    /// 服务状态迁移的**唯一推送点**（§5.2）。
    ///
    /// 先更新权威 provider，再刷新菜单项文字。任何改变服务状态的路径都必须走这里，
    /// 否则菜单会一直显示陈旧状态——这正是「`refresh_status` 有定义但零调用」的成因。
    pub fn set_service_status(&self, status: LocalServiceStatus) {
        self.service.read().unwrap().status.set(status);
        self.sync_tray_status();
    }

    /// 按当前权威状态刷新常驻菜单的状态项（文案端口一律取自状态本身，§9.2）
    pub fn sync_tray_status(&self) {
        let label = self.service.read().unwrap().status.snapshot().menu_label();
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
        tray::ID_OPEN_DOWNLOADS => open_downloads_dir_for_tray(app),
        // 只读 / 未启用项不应产生事件
        tray::ID_SERVICE_STATUS => log::warn!("service_status 为只读项，不应触发事件"),
        tray::ID_OPEN_SERVICE_SETTINGS => log::warn!("本地服务设置尚未启用（阶段 B）"),
        tray::ID_QUIT => request_quit(app, QuitSource::Menu),
        // macOS 应用菜单的 Quit（⌘Q）：与托盘 Quit 同语义（§7.1），只是来源不同
        tray::ID_APP_QUIT => request_quit(app, QuitSource::Shortcut),
        other => log::warn!("未知菜单项: {other}"),
    }
}

/// 打开下载目录（§9.5 / A11 托盘菜单路径）：不存在则创建；任何失败都恢复主窗口暴露错误，不静默失败
fn open_downloads_dir_for_tray(app: &tauri::AppHandle) {
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
/// 阶段 B 起传输快照来自真实引擎（`EngineTransferAdapter`），播种 TransferSnapshot
/// 的旧路径已移除；真实传输任务可直接通过前端或 `QINGNIAO_DEBUG_TRANSFER=1` 时
/// 的状态播种来驱动退出确认分支。本函数现仅保留状态播种占位（A12 由真实任务覆盖）。
#[cfg(debug_assertions)]
fn seed_debug_state_from_env(_app: &tauri::AppHandle) {
    let Ok(_raw) = std::env::var("QINGNIAO_DEBUG_TRANSFER") else {
        return;
    };
    log::warn!("QINGNIAO_DEBUG_TRANSFER 已忽略：传输快照现由真实引擎提供");
}

/// 最终清理：由退出协议在 `Draining` 阶段调用（§7.2.2 第 3.4 / 3.5 步与第 4 步）
struct AppCleanup {
    app: tauri::AppHandle,
}

impl FinalCleanup for AppCleanup {
    fn stop_listener(&self) {
        // 阶段 B：真实 HTTP listener 停机与端口释放（§10.1 退出门闩由 stop_accepting 先行）
        let state = self.app.state::<AppState>();
        let svc = state.service.read().unwrap();
        svc.service.stop();
        svc.status.set(LocalServiceStatus::Stopped);
        // 状态迁移后立即推送菜单文案（§5.2）
        state.sync_tray_status();
        log::info!("本地服务已停止: {:?}", svc.status.snapshot());
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
    let svc = state.service.read().unwrap();
    let step = state.exit.after_confirm(
        confirmed,
        svc.service.as_ref(),
        svc.transfer.as_ref(),
        svc.transfer_ctrl.as_ref(),
        &cleanup,
    );
    report_quit_step(app, step);
}

/// 统一退出入口：所有显式退出都必须走这里（§7.1）
pub fn request_quit(app: &tauri::AppHandle, source: QuitSource) {
    let state = app.state::<AppState>();
    let cleanup = AppCleanup { app: app.clone() };
    let svc = state.service.read().unwrap();
    let step = state.exit.begin_quit(
        source,
        svc.service.as_ref(),
        svc.transfer.as_ref(),
        svc.transfer_ctrl.as_ref(),
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
            delete_history_item,
            append_history_item,
            send_webhook,
            upload_image,
            test_connection,
            log_event,
            read_file_base64,
            analyze,
            send_message,
            resend_payload,
            skills_status,
            install_skills,
            uninstall_skills,
            cli_install_status,
            install_cli,
            about_info,
            key_status,
            key_generate,
            key_export,
            key_import,
            key_rotate,
            transfer_start_upload,
            transfer_cancel,
            transfer_parse_payload,
            transfer_confirm_download,
            transfer_task_state,
            transfer_service_status,
            transfer_service_restart,
            open_download_dir,
            // 仅 debug：A8 状态刷新的驱动入口
            #[cfg(debug_assertions)]
            debug_set_service_status
        ])
        .setup(|app| {
            // 先装 logger，之后的 tray / 生命周期日志才有处可去
            install_file_logger(app.handle());
            log::info!("青鸟启动：单实例判定通过，开始初始化常驻功能");

            // ===== Agent 技能 drift 自动更新（方案 v3 §七 D13）=====
            {
                let app_handle = app.handle().clone();
                std::thread::Builder::new()
                    .name("qn-skills-autoupdate".into())
                    .spawn(move || {
                        // 源缺失（未内置 skills/ 的安装包）直接跳过，不必刷 WARN
                        let Some(dir) = bundled_skills_dir(&app_handle) else { return };
                        let targets = skill_targets();
                        match qingniao_core::skills::auto_update(&dir, &targets) {
                            Ok(true) => log::info!("Agent 技能检测到漂移，已自动更新"),
                            Ok(false) => {}
                            Err(e) => log::warn!("Agent 技能自动更新失败: {e}"),
                        }
                    })
                    .ok();
            }

            // ===== 传输功能装配（dock-tray 阶段 B）=====
            // 1. 引擎：<app_config_dir>/transfer（quota.json / consumed.json 等持久化在此）
            let configured_port = {
                let cfg_path = app.path().app_config_dir()
                    .map_err(|e| format!("无法定位配置目录: {e}"))?
                    .join("qingniao.json");
                std::fs::read_to_string(&cfg_path)
                    .ok()
                    .and_then(|s| serde_json::from_str::<serde_json::Value>(&s).ok())
                    .and_then(|v| v.pointer("/transfer/configured_port").and_then(|x| x.as_u64()))
                    .map(|v| v as u16)
            };
            let work_dir = app.path().app_config_dir()
                .map_err(|e| format!("无法定位配置目录: {e}"))?
                .join("transfer");
            let engine = transfer::engine::Engine::open(
                work_dir,
                transfer::TauriHost::into_arc(app.handle().clone()),
            )
                .map_err(|e| -> Box<dyn std::error::Error> { e.into() })?;
            let engine = std::sync::Arc::new(engine);
            let _ = app.state::<AppState>().engine.set(engine.clone());

            // 2. 真实本地服务 + 状态源（StatusCell）
            let cell = std::sync::Arc::new(service::StatusCell::new(LocalServiceStatus::Starting));
            let real = local_server::RealLocalService::new(cell.clone(), engine.clone());
            let _ = app.state::<AppState>().real_service.set(real.clone());

            // 3. 启动本地服务（端口占用不漂移，落 Failed{PortInUse}）
            let port = configured_port.unwrap_or(DEFAULT_LOCAL_PORT);
            let page = include_str!("../assets/transfer-confirm.html");
            let start_status = local_server::restart_service(&real, port, page);
            app.state::<AppState>().set_service_status(start_status);

            // 4. 替换 AppState.service 为真实集合（退出协议 / 菜单走真实现）
            let adapter = std::sync::Arc::new(EngineTransferAdapter(engine.clone()));
            *app.state::<AppState>().service.write().unwrap() = service::PhaseAService::with_parts(
                cell,
                real,
                adapter.clone(),
                adapter,
            );
            log::info!("传输功能装配完成：configured_port={port}");

            // 5. 删除重试队列兜底（§9.4 幂等恢复）
            {
                let cfg = read_app_config(app.handle());
                engine.retry_pending_deletes(&cfg.app_id, &cfg.app_secret);
            }

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
mod agent_install_tests {
    use super::*;

    fn tmp_skills(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("qn-agent-install-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// 技能源探测：跳过缺失候选，取第一个真实含 skills/qingniao 的目录
    #[test]
    fn pick_skills_dir_skips_missing_candidates() {
        let base = tmp_skills("pick");
        let bogus = base.join("no-such-dir");
        let blank = base.join("blank");
        std::fs::create_dir_all(&blank).unwrap(); // 有 skills/ 但没有 skills/qingniao
        std::fs::create_dir_all(blank.join("skills")).unwrap();
        let real = base.join("portable").join("skills"); // 候选就是 skills/ 目录本身
        std::fs::create_dir_all(real.join(qingniao_core::skills::SKILL_NAME)).unwrap();

        assert_eq!(
            pick_skills_dir(vec![bogus.clone(), real.clone()]),
            Some(real),
            "缺失候选必须被跳过（Windows 便携包场景）"
        );
        assert_eq!(
            pick_skills_dir(vec![bogus, blank]),
            None,
            "没有候选命中 → None，交由 UI 显示未打包"
        );
        let _ = std::fs::remove_dir_all(&base);
    }

    /// 最近更新 = build.rs 写入的打包时刻，按本地时区格式化成 16 字符 `YYYY-MM-DD HH:MM`
    #[test]
    fn build_time_label_reflects_build_epoch() {
        let s = build_time_label();
        assert_eq!(s.len(), 16, "应为 YYYY-MM-DD HH:MM，实际：{s}");
        assert_eq!(&s[4..5], "-");
        assert_eq!(&s[10..11], " ");
        assert!(s.starts_with("20"), "年份异常：{s}");
        // build.rs 必须真的写入了非零时刻（否则 About 会显示占位符）
        let epoch: i64 = option_env!("QINGNIAO_BUILD_EPOCH")
            .expect("build.rs 应写入 QINGNIAO_BUILD_EPOCH")
            .parse()
            .expect("QINGNIAO_BUILD_EPOCH 应为整数秒");
        assert!(epoch > 0, "打包时刻不应为 0：{epoch}");
    }

    /// 数据目录缩写：HOME/APPDATA 前缀换成短标签，不匹配则原样返回
    #[test]
    fn abbrev_path_shortens_known_prefix() {
        let home = PathBuf::from("/home/one");
        assert_eq!(
            abbrev_path(&home.join("Library/Application Support/qingniao"), Some(&home), "~"),
            "~/Library/Application Support/qingniao"
        );
        assert_eq!(abbrev_path(&home, Some(&home), "~"), "~");
        assert_eq!(abbrev_path(&home.join("x"), None, "~"), "/home/one/x");
        assert_eq!(abbrev_path(Path::new("/other/p"), Some(&home), "~"), "/other/p");
    }
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
