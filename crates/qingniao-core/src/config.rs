//! 配置持久层：APP 与 CLI 共用 `qingniao.json`（方案 v3 §五，D3–D7）。
//!
//! 设计要点：
//! - **raw 透传**：内部以 `serde_json::Map` 持有整个配置文档，序列化原样写回，
//!   从根上保证「APP 新增字段 → CLI 保存 → 字段仍在」（D5），history 条目同理逐条保真
//! - **锁纪律**（D3）：`<config>.lock` advisory lock，写独占 / 读共享，超时 5s；
//!   读是单次快照，写是锁内 read-modify-write
//! - **原子替换**（D4）：唯一临时文件（pid 后缀）→ fsync → rename；
//!   `std::fs::rename` 在 Windows 为 MOVEFILE_REPLACE_EXISTING 语义，禁止先删后改名
//! - **last-write-wins 为已知限制**：APP 持有内存配置全量保存仍可能覆盖 CLI 写入

use serde_json::{json, Map, Value};
use std::fs::File;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

pub const CONFIG_FILE_NAME: &str = "qingniao.json";
/// 旧版配置目录（identifier 曾为 com.qingniao.app），一次性迁移来源
pub const OLD_CONFIG_DIR: &str = "com.qingniao.app";
const LOCK_TIMEOUT: Duration = Duration::from_secs(5);
const HISTORY_CAP: usize = 100;
/// URL 白名单（D10）：CLI 默认仅允许 https + 飞书域名
pub const URL_HOST_ALLOWLIST: [&str; 2] = ["open.feishu.cn", "open.larksuite.com"];

pub const ERR_CONFIG_BUSY: &str = "配置正被其他进程修改，请稍后重试";

// ===== 路径解析 =====

/// 平台默认配置目录（与 Tauri app_config_dir 一致；CLI 不调 Tauri API，硬编码平台规则）
pub fn default_config_dir() -> Result<PathBuf, String> {
    if cfg!(target_os = "macos") {
        let home = std::env::var_os("HOME").ok_or("无法定位 HOME 目录")?;
        Ok(Path::new(&home)
            .join("Library")
            .join("Application Support")
            .join("qingniao"))
    } else if cfg!(target_os = "windows") {
        let appdata = std::env::var_os("APPDATA").ok_or("无法定位 %APPDATA% 目录")?;
        Ok(Path::new(&appdata).join("qingniao"))
    } else {
        let home = std::env::var_os("HOME").ok_or("无法定位 HOME 目录")?;
        Ok(Path::new(&home).join(".config").join("qingniao"))
    }
}

/// 解析配置目录与文件路径；`QINGNIAO_CONFIG_DIR` 可覆盖（测试 / 多实例）
pub fn resolve_config_path() -> Result<PathBuf, String> {
    let dir = match std::env::var_os("QINGNIAO_CONFIG_DIR") {
        Some(d) => PathBuf::from(d),
        None => default_config_dir()?,
    };
    config_path_in_dir(&dir)
}

/// 在指定目录内解析配置路径（create_dir_all + 旧目录迁移），供测试与多实例直接驱动
pub fn config_path_in_dir(dir: &Path) -> Result<PathBuf, String> {
    std::fs::create_dir_all(dir).map_err(|e| format!("无法创建配置目录: {e}"))?;
    let path = dir.join(CONFIG_FILE_NAME);
    migrate_old_config(dir, &path)?;
    Ok(path)
}

fn migrate_old_config(dir: &Path, new_path: &Path) -> Result<(), String> {
    if new_path.exists() {
        return Ok(());
    }
    let old_path = dir
        .parent()
        .map(|p| p.join(OLD_CONFIG_DIR).join(CONFIG_FILE_NAME));
    let Some(old_path) = old_path else {
        return Ok(());
    };
    if !old_path.exists() {
        return Ok(());
    }
    std::fs::copy(&old_path, new_path).map_err(|e| format!("迁移旧配置失败: {e}"))?;
    Ok(())
}

// ===== 文件锁（std 1.89 稳定的 advisory file lock）=====

struct FileLockGuard {
    file: File,
}

impl Drop for FileLockGuard {
    fn drop(&mut self) {
        // std 只提供 unlock()（释放任意类型的锁）
        let _ = self.file.unlock();
    }
}

fn acquire_lock(config_path: &Path, shared: bool) -> Result<FileLockGuard, String> {
    let lock_path = config_path.with_extension("json.lock");
    let file = File::create(&lock_path)
        .map_err(|e| format!("无法创建锁文件 {}: {e}", lock_path.display()))?;
    let deadline = Instant::now() + LOCK_TIMEOUT;
    loop {
        let res = if shared {
            file.try_lock_shared()
        } else {
            file.try_lock()
        };
        match res {
            Ok(()) => return Ok(FileLockGuard { file }),
            Err(_) if Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(50));
            }
            Err(_) => return Err(ERR_CONFIG_BUSY.to_string()),
        }
    }
}

/// 锁内执行（写 = 独占，读 = 共享）
pub fn with_lock<T>(
    config_path: &Path,
    shared: bool,
    f: impl FnOnce() -> Result<T, String>,
) -> Result<T, String> {
    let _guard = acquire_lock(config_path, shared)?;
    f()
}

// ===== Config（raw 文档模型）=====

/// 整个配置文档的内存表示。字段访问全部走解析器，未知字段原样保留。
#[derive(Debug, Clone)]
pub struct Config {
    pub raw: Map<String, Value>,
}

/// 机器人视图（webhooks 数组元素解析结果）
#[derive(Debug, Clone, PartialEq)]
pub struct BotView {
    pub index: usize,
    /// schema v2 起存在；旧配置缺失时由 `ensure_bot_ids` 在写路径补齐
    pub id: Option<String>,
    pub name: String,
    pub url: String,
    pub secret: String,
}

impl BotView {
    /// `-b` 解析顺序：id > 名称 > 下标（D6）。下标从 1 开始（面向人类；旧配置的
    /// bot1/bot2… 稳定 id 同为 1 基，两者天然对齐）
    pub fn matches_key(&self, key: &str) -> bool {
        if let Some(id) = &self.id {
            if id == key {
                return true;
            }
        }
        if self.name == key {
            return true;
        }
        (self.index + 1).to_string() == key
    }
    /// 脱敏 webhook：保留 scheme+host+路径前缀，hook id 打码
    pub fn url_masked(&self) -> String {
        mask_webhook_url(&self.url)
    }
    pub fn has_secret(&self) -> bool {
        !self.secret.is_empty()
    }
}

pub fn mask_webhook_url(url: &str) -> String {
    // https://open.feishu.cn/open-apis/bot/v2/hook/xxxx-xxxx → .../hook/****（保留前 4 位）
    match url.find("hook/") {
        Some(i) => {
            let rest = &url[i + 5..];
            let keep = rest.chars().take(4).collect::<String>();
            format!("{}{}****", &url[..i + 5], keep)
        }
        None => url.to_string(),
    }
}

#[derive(Debug, Clone)]
pub struct LoadedConfig {
    pub config: Config,
    /// 权限等非致命告警（如 world-readable）
    pub warnings: Vec<String>,
}

impl Config {
    pub fn default_document() -> Map<String, Value> {
        // 与 APP 前端初始状态一致（main.js：webhooks:[], history:[], last_webhook:0, last_type:'auto'）
        json!({
            "webhooks": [],
            "history": [],
            "last_webhook": 0,
            "last_type": "auto",
            "schema_version": 2,
        })
        .as_object()
        .cloned()
        .expect("默认文档必须是对象")
    }

    pub fn empty() -> Config {
        Config {
            raw: Config::default_document(),
        }
    }

    fn parse(raw: Value) -> Result<Config, String> {
        let obj = raw
            .as_object()
            .cloned()
            .ok_or("配置文件不是 JSON 对象")?;
        Ok(Config { raw: obj })
    }

    pub fn schema_version(&self) -> u32 {
        self.raw.get("schema_version").and_then(|v| v.as_u64()).unwrap_or(1) as u32
    }

    pub fn webhooks(&self) -> Vec<BotView> {
        self.raw
            .get("webhooks")
            .and_then(|v| v.as_array())
            .map(|arr| {
                arr.iter()
                    .enumerate()
                    .map(|(index, w)| BotView {
                        index,
                        id: w.get("id").and_then(|v| v.as_str()).map(String::from),
                        name: w.get("name").and_then(|v| v.as_str()).unwrap_or("").to_string(),
                        url: w.get("url").and_then(|v| v.as_str()).unwrap_or("").to_string(),
                        secret: w.get("secret").and_then(|v| v.as_str()).unwrap_or("").to_string(),
                    })
                    .collect()
            })
            .unwrap_or_default()
    }

    pub fn history(&self) -> &[Value] {
        static EMPTY: [Value; 0] = [];
        self.raw
            .get("history")
            .and_then(|v| v.as_array())
            .map(|a| a.as_slice())
            .unwrap_or(&EMPTY)
    }

    /// 默认机器人：last_bot_id > last_webhook 下标 > 第一个（D6）
    pub fn default_bot(&self) -> Option<BotView> {
        let bots = self.webhooks();
        if bots.is_empty() {
            return None;
        }
        if let Some(id) = self.raw.get("last_bot_id").and_then(|v| v.as_str()) {
            if let Some(b) = bots.iter().find(|b| b.id.as_deref() == Some(id)) {
                return Some(b.clone());
            }
        }
        let idx = self.raw.get("last_webhook").and_then(|v| v.as_u64()).unwrap_or(0) as usize;
        bots.get(idx).cloned().or_else(|| bots.first().cloned())
    }

    pub fn find_bot(&self, key: &str) -> Option<BotView> {
        self.webhooks().into_iter().find(|b| b.matches_key(key))
    }

    /// 写路径调用：为缺失 id 的机器人补齐稳定 id（旧配置按 bot{index+1}，新增用随机 b-xxxx）
    pub fn ensure_bot_ids(&mut self) {
        let Some(arr) = self.raw.get_mut("webhooks").and_then(|v| v.as_array_mut()) else {
            return;
        };
        for (i, w) in arr.iter_mut().enumerate() {
            if w.get("id").and_then(|v| v.as_str()).is_none() {
                if let Some(obj) = w.as_object_mut() {
                    obj.insert("id".into(), Value::String(format!("bot{}", i + 1)));
                }
            }
        }
    }

    pub fn set_last_bot(&mut self, id: &str, index: usize) {
        self.raw.insert("last_bot_id".into(), json!(id));
        // 双写过渡：旧 APP 只认 last_webhook 下标
        self.raw.insert("last_webhook".into(), json!(index));
    }

    pub fn add_bot(&mut self, name: &str, url: &str, secret: &str) -> Result<String, String> {
        if name.trim().is_empty() {
            return Err("机器人名称不能为空".into());
        }
        let bots = self.webhooks();
        if bots.iter().any(|b| b.name == name.trim()) {
            return Err(format!("已存在同名机器人: {name}"));
        }
        let id = format!("b-{}", random_hex(8));
        self.raw
            .get_mut("webhooks")
            .and_then(|v| v.as_array_mut())
            .ok_or("配置中 webhooks 不是数组")?
            .push(json!({
                "id": id,
                "name": name.trim(),
                "url": url.trim(),
                "secret": secret,
            }));
        self.ensure_bot_ids();
        Ok(id)
    }

    pub fn remove_bot(&mut self, key: &str) -> Result<BotView, String> {
        let bots = self.webhooks();
        let target = bots
            .iter()
            .find(|b| b.matches_key(key))
            .ok_or_else(|| format!("未找到机器人: {key}"))?;
        let arr = self
            .raw
            .get_mut("webhooks")
            .and_then(|v| v.as_array_mut())
            .ok_or("配置中 webhooks 不是数组")?;
        arr.remove(target.index);
        self.ensure_bot_ids();
        // 删除的是默认机器人时回退第一个（D6）
        let still_default = self
            .raw
            .get("last_bot_id")
            .and_then(|v| v.as_str())
            .map(|id| id == target.id.as_deref().unwrap_or_default())
            .unwrap_or(false);
        if still_default {
            self.raw.remove("last_bot_id");
            self.raw.insert("last_webhook".into(), json!(0));
        }
        Ok(target.clone())
    }

    /// 追加发送历史（D7：history 只由 core 的 record_send 写入；上限 100 条与 APP 一致）
    pub fn push_history(&mut self, rec: Value) {
        let arr = self
            .raw
            .entry("history")
            .or_insert_with(|| json!([]));
        if !arr.is_array() {
            *arr = json!([]);
        }
        let a = arr.as_array_mut().unwrap();
        a.push(rec);
        if a.len() > HISTORY_CAP {
            let overflow = a.len() - HISTORY_CAP;
            a.drain(0..overflow);
        }
    }
}

// ===== 加载 / 保存 =====

/// 读快照（共享锁内单次读取）
pub fn load(config_path: &Path) -> Result<LoadedConfig, String> {
    with_lock(config_path, true, || {
        load_unlocked(config_path)
    })
}

fn load_unlocked(config_path: &Path) -> Result<LoadedConfig, String> {
    let mut warnings = Vec::new();
    if !config_path.exists() {
        return Ok(LoadedConfig {
            config: Config::empty(),
            warnings,
        });
    }
    let mut data = String::new();
    File::open(config_path)
        .and_then(|mut f| f.read_to_string(&mut data))
        .map_err(|e| format!("读取配置失败: {e}"))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if let Ok(meta) = std::fs::metadata(config_path) {
            let mode = meta.permissions().mode();
            if mode & 0o077 != 0 {
                warnings.push(format!(
                    "配置文件权限过宽（{:o}），应为 600；已尝试收紧",
                    mode & 0o777
                ));
                let _ = std::fs::set_permissions(config_path, std::fs::Permissions::from_mode(0o600));
            }
        }
    }
    let cfg = Config::parse(serde_json::from_str(&data).map_err(|e| format!("配置解析失败: {e}"))?)?;
    Ok(LoadedConfig { config: cfg, warnings })
}

/// 锁内 read-modify-write：`f` 在最新快照上原地修改，随后原子落盘。
/// 所有写操作（bot 增删改、history 追加）都必须走这里。
pub fn modify<T>(
    config_path: &Path,
    f: impl FnOnce(&mut Config) -> Result<T, String>,
) -> Result<T, String> {
    with_lock(config_path, false, || {
        let mut loaded = load_unlocked(config_path)?;
        let out = f(&mut loaded.config)?;
        save_unlocked(config_path, &loaded.config)?;
        Ok(out)
    })
}

fn save_unlocked(config_path: &Path, config: &Config) -> Result<(), String> {
    let mut doc = config.raw.clone();
    doc.insert("schema_version".into(), json!(2));
    let data = serde_json::to_string_pretty(&Value::Object(doc))
        .map_err(|e| format!("配置序列化失败: {e}"))?;
    // 唯一临时文件名（pid 后缀），避免固定 tmp 两进程互踩（D4）
    let tmp = config_path.with_extension(format!("json.tmp.{}", std::process::id()));
    {
        let mut f = File::create(&tmp).map_err(|e| format!("写入临时文件失败: {e}"))?;
        f.write_all(data.as_bytes())
            .and_then(|_| f.flush())
            .and_then(|_| f.sync_all())
            .map_err(|e| {
                let _ = std::fs::remove_file(&tmp);
                format!("写入配置失败: {e}")
            })?;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o600));
    }
    // rename 直接覆盖已存在文件（Windows 为 MOVEFILE_REPLACE_EXISTING）；失败保留 tmp 供诊断
    std::fs::rename(&tmp, config_path).map_err(|e| {
        format!(
            "保存配置失败: {e}（临时文件保留于 {}）",
            tmp.display()
        )
    })?;
    Ok(())
}

// ===== URL 策略（D10）=====

/// CLI 默认安全模式：https + 飞书域名；`--allow-insecure-url` 显式放开
pub fn check_url_policy(url: &str, allow_insecure: bool) -> Result<(), String> {
    let trimmed = url.trim();
    let (scheme, rest) = trimmed.split_once("://").ok_or_else(|| {
        format!("Webhook 地址缺少协议（须以 https:// 开头）: {}", mask_webhook_url(trimmed))
    })?;
    let host = rest.split('/').next().unwrap_or("").to_lowercase();
    let host = host.split(':').next().unwrap_or(&host).to_string();
    if scheme.eq_ignore_ascii_case("https") && URL_HOST_ALLOWLIST.contains(&host.as_str()) {
        return Ok(());
    }
    if allow_insecure {
        if scheme.eq_ignore_ascii_case("http") || scheme.eq_ignore_ascii_case("https") {
            return Ok(());
        }
        return Err(format!("不支持的协议: {scheme}://（仅允许 http/https）"));
    }
    Err(format!(
        "目标地址不在默认白名单内（仅允许 https://{}/ 或 https://{}/）。\
         内网 mock / 自建网关请显式加 --allow-insecure-url",
        URL_HOST_ALLOWLIST[0], URL_HOST_ALLOWLIST[1]
    ))
}

// ===== 随机 id =====

pub fn random_hex(n_bytes: usize) -> String {
    // CSPRNG：/dev/urandom（unix）+ 时间熵兜底（Windows / 测试环境可接受退化）
    let mut bytes = vec![0u8; n_bytes];
    #[cfg(unix)]
    {
        if let Ok(mut f) = File::open("/dev/urandom") {
            if f.read_exact(&mut bytes).is_ok() {
                return bytes.iter().map(|b| format!("{b:02x}")).collect();
            }
        }
    }
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let seed = nanos as u64 ^ (std::process::id() as u64) << 32;
    for (i, b) in bytes.iter_mut().enumerate() {
        *b = (seed.wrapping_mul(0x9E3779B97F4A7C15).wrapping_shr(i as u32 * 7) & 0xFF) as u8;
    }
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mask_keeps_prefix() {
        assert_eq!(
            mask_webhook_url("https://open.feishu.cn/open-apis/bot/v2/hook/abcd-1234-efgh"),
            "https://open.feishu.cn/open-apis/bot/v2/hook/abcd****"
        );
        assert_eq!(mask_webhook_url("https://example.com/hookless"), "https://example.com/hookless");
    }

    #[test]
    fn url_policy() {
        assert!(check_url_policy("https://open.feishu.cn/open-apis/bot/v2/hook/x", false).is_ok());
        assert!(check_url_policy("https://open.larksuite.com/open-apis/bot/v2/hook/x", false).is_ok());
        assert!(check_url_policy("http://127.0.0.1:8899/hook", false).is_err());
        assert!(check_url_policy("http://127.0.0.1:8899/hook", true).is_ok());
        assert!(check_url_policy("ftp://x", true).is_err());
    }

    #[test]
    fn bot_key_resolution_order() {
        let mut cfg = Config::empty();
        cfg.add_bot("alpha", "https://open.feishu.cn/hook/a", "").unwrap();
        cfg.add_bot("beta", "https://open.feishu.cn/hook/b", "").unwrap();
        assert_eq!(cfg.find_bot("2").unwrap().name, "beta");
        assert_eq!(cfg.find_bot("beta").unwrap().name, "beta");
        let id = cfg.webhooks()[0].id.clone().unwrap();
        assert_eq!(cfg.find_bot(&id).unwrap().name, "alpha");
    }
}
