//! 传输引擎：任务生命周期 / worker 线程 / 进度 emit / 取消 / 下载会话（§7.5/§9/§10.1）。
//!
//! 并发模型（方案定案）：std::thread + blocking reqwest；每任务一线程，
//! 任务内 `Arc<AtomicBool>` CancelToken，在片边界检查取消。
//! 进度事件 `transfer://progress`，payload：
//! `{task_id, dir, state, bytes_done, bytes_total, chunk_index, chunk_total, rate_bps, error?, link?, final_path?}`
//! state ∈ running | done | failed | cancelled。

use crate::keyring_store;
use crate::transfer::crypto::{
    self, open_chunk, open_payload, seal_chunk, seal_payload, validate_metadata, ChunkMeta,
    Envelope, Metadata, CHUNK_SIZE, MAX_FILE_SIZE, NONCE_LEN,
};
use crate::transfer::feishu::{build_transfer_card, send_webhook_json, FeishuClient};
use crate::transfer::quota::Quota;
use base64::Engine as _;
use rand::RngCore;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;

/* ===================== 宿主接口（M0a：全部 tauri 耦合收敛到这一点） ===================== */

/// 本地服务默认端口（自 src-tauri/service.rs 下沉，M0c：链接端口来源改为配置值）
pub const DEFAULT_LOCAL_PORT: u16 = 9876;

/// 引擎与宿主环境的**唯一**接口。
///
/// APP 传 Tauri 实现（`src-tauri/src/transfer/mod.rs` 的 `TauriHost`）；
/// CLI 与测试传 [`FixedHost`]（固定值 + 事件丢弃）。
///
/// 这样 core 保持零 tauri 依赖，同时 APP 侧行为不变——原先直接调 `AppHandle` 的
/// 五处（配置目录 / 下载目录 / 本地服务端口 / 进度事件 / 完成事件）逐一对应到本 trait。
pub trait Host: Send + Sync {
    /// `qingniao.json` 所在目录
    fn config_dir(&self) -> Result<PathBuf, String>;
    /// 下载落地目录（= 配置项，空则系统下载目录；不存在时创建）
    fn resolve_download_dir(&self) -> Result<String, String>;
    /// 链接端口口径（D23/M0c）：写入取回链接的 configured_port——
    /// 接收端据它拨号，与发送端本机服务的运行时状态无关
    fn configured_port(&self) -> u16;
    /// 本机本地服务当前实际绑定的端口；`None` = 未运行
    fn service_bound_port(&self) -> Option<u16>;
    /// 进度事件（`transfer://progress`）
    fn progress(&self, payload: serde_json::Value);
    /// 下载完成事件（`transfer://downloaded`）
    fn downloaded(&self, payload: serde_json::Value);
}

/// CLI / 测试用宿主：固定值 + 事件丢弃。
pub struct FixedHost {
    pub config_dir: PathBuf,
    /// `None` 或空串 ⇒ `$HOME/Downloads`
    pub download_dir: Option<String>,
    pub service_port: Option<u16>,
    /// 写进取回链接的端口（D23）；缺省 [`DEFAULT_LOCAL_PORT`]
    pub configured_port: u16,
}

impl FixedHost {
    pub fn new(config_dir: PathBuf) -> Self {
        Self { config_dir, download_dir: None, service_port: None, configured_port: DEFAULT_LOCAL_PORT }
    }
    pub fn with_download_dir(mut self, dir: impl Into<String>) -> Self {
        self.download_dir = Some(dir.into());
        self
    }
    pub fn with_service_port(mut self, port: Option<u16>) -> Self {
        self.service_port = port;
        self
    }
    pub fn with_configured_port(mut self, port: u16) -> Self {
        self.configured_port = port;
        self
    }
}

impl Host for FixedHost {
    fn config_dir(&self) -> Result<PathBuf, String> { Ok(self.config_dir.clone()) }

    fn resolve_download_dir(&self) -> Result<String, String> {
        let dir = match self.download_dir.as_deref() {
            Some(d) if !d.is_empty() => PathBuf::from(d),
            // 不依赖宿主 API 的兜底（CLI 场景）；APP 侧仍走 Tauri 的系统下载目录解析
            _ => {
                let home = std::env::var_os("HOME").ok_or("无法定位 HOME 目录")?;
                PathBuf::from(home).join("Downloads")
            }
        };
        std::fs::create_dir_all(&dir).map_err(|e| format!("创建下载目录失败: {e}"))?;
        Ok(dir.to_string_lossy().to_string())
    }

    fn configured_port(&self) -> u16 { self.configured_port }
    fn service_bound_port(&self) -> Option<u16> { self.service_port }
    fn progress(&self, _payload: serde_json::Value) {}
    fn downloaded(&self, _payload: serde_json::Value) {}
}

/* ===================== 密钥来源（U7 注入接缝） ===================== */

/// 传输密钥来源。生产实现读 OS 凭据库（[`KeyringKeys`]）；
/// 测试 / CI 注入固定值——否则构造过期 payload 就得写真实主密钥条目（U7，方案 §15「排序修正」）。
/// M3 的「keyring 抽 trait」在此接缝上正式化。
pub trait KeySource: Send + Sync {
    fn current(&self) -> Result<Option<String>, String>;
    fn previous(&self) -> Result<Option<String>, String>;
}

/// 生产实现：OS 凭据库（`keyring_store`，SERVICE `com.qingniao.transfer`）
pub struct KeyringKeys;

impl KeySource for KeyringKeys {
    fn current(&self) -> Result<Option<String>, String> { keyring_store::get_current() }
    fn previous(&self) -> Result<Option<String>, String> { keyring_store::get_previous() }
}

/* ===================== 分片云端存取（U13 可控失败点） ===================== */

/// 分片的云端拉取 / 删除。生产实现走 [`FeishuClient`]（私有 [`FeishuChunks`] 包装）；
/// 测试 / CI 注入脚本化实现——即方案 P2-6 要求的 U13「本地 stub 最小形态」。
pub trait ChunkStore: Send + Sync {
    /// 拉取密文分片
    fn fetch(&self, token: &str) -> Result<Vec<u8>, String>;
    /// 删除云端分片（下载即删 D11；Err 进重试队列）
    fn delete(&self, token: &str) -> Result<(), String>;
}

/// 生产实现：FeishuClient 的分片读写（错误映射与去 tauri 化前逐字一致）
struct FeishuChunks {
    client: FeishuClient,
}

impl ChunkStore for FeishuChunks {
    fn fetch(&self, token: &str) -> Result<Vec<u8>, String> {
        self.client.download_chunk(token).map_err(|e| {
            if e.http_status == 404 {
                "分片已被删除（可能已被取回或清理），请对方重新上传".to_string()
            } else if e.http_status == 403 {
                "无下载权限：请检查应用权限配置".to_string()
            } else {
                e.to_string()
            }
        })
    }
    fn delete(&self, token: &str) -> Result<(), String> {
        self.client.delete_file(token).map_err(|e| e.to_string())
    }
}

/* ===================== 指纹锁（M0b：D24 跨进程单消费） ===================== */

/// per-指纹 OS advisory lock：`<work_dir>/<指纹>.lock`。
/// 锁顺序（§7 实现约束②）：指纹锁 → 状态文件锁，单向；guard 由
/// `PendingDownload` 携带、`run_download_sync` 取出并持有到下载结束（实现约束①）。
pub struct FingerprintLock {
    /// 持有即持锁；字段本身无需读取（flock 生命周期绑定 fd）
    #[allow(dead_code)]
    file: File,
}

impl FingerprintLock {
    /// 非阻塞获取；已被其他进程/任务持有 → `Ok(None)`（claim 映射为 `Busy`，U17 409）
    fn try_acquire(work_dir: &std::path::Path, fingerprint: &str) -> Result<Option<Self>, String> {
        let path = work_dir.join(format!("{fingerprint}.lock"));
        let f = OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(&path)
            .map_err(|e| format!("创建指纹锁失败: {e}"))?;
        match f.try_lock() {
            Ok(()) => Ok(Some(Self { file: f })),
            Err(std::fs::TryLockError::WouldBlock) => Ok(None),
            Err(std::fs::TryLockError::Error(e)) => Err(format!("指纹锁加锁失败: {e}")),
        }
    }

    /// 清理孤儿锁文件（实现约束③，Engine::open 时执行）：
    /// flock 随进程消亡，因此「当前能独占锁住」的指纹锁必然无人持有 → 删除。
    ///
    /// **只清指纹锁**（D33）：文件名必须是 `<64 位小写 hex>.lock`
    /// （payload_fingerprint = sha256 hex）。statefile 的锁专用旁文件
    /// `<name>.json.lock` 同样以 `.lock` 结尾——放宽匹配会把这 4 个常驻锁文件
    /// 当孤儿扫掉，并在「他人 open 之后、try_lock 之前」的窗口拆锁、破坏互斥。
    fn sweep_orphans(work_dir: &std::path::Path) {
        let Ok(entries) = std::fs::read_dir(work_dir) else { return };
        for entry in entries.flatten() {
            let path = entry.path();
            if !is_fingerprint_lock_path(&path) {
                continue;
            }
            if let Ok(f) = OpenOptions::new().write(true).open(&path) {
                if f.try_lock().is_ok() {
                    let _ = std::fs::remove_file(&path);
                    // f drop → 解锁
                }
            }
        }
    }
}

/// 指纹锁路径判定：`<64 位小写 hex>.lock`。**不得放宽**——
/// statefile 的锁专用文件 `quota.json.lock` / `consumed.json.lock` /
/// `pending_deletes.json.lock` / `cleanup.json.lock`（D33）都不得被当孤儿扫掉。
fn is_fingerprint_lock_path(path: &std::path::Path) -> bool {
    let Some(name) = path.file_name().and_then(|n| n.to_str()) else { return false };
    let Some(stem) = name.strip_suffix(".lock") else { return false };
    stem.len() == 64
        && stem
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

/// 单任务控制块
pub struct TaskHandle {
    pub cancel: Arc<AtomicBool>,
    #[allow(dead_code)]
    pub dir: &'static str, // "out" | "in"
    /// 指纹锁 guard（M0b）：claim 成功即存入，`run_download_sync` 取出并持有到下载结束。
    /// 放在 TaskHandle（不可克隆、Arc 共享）而非 PendingDownload（Clone）以保证独占语义。
    pub fp_lock: Mutex<Option<FingerprintLock>>,
}

pub struct Engine {
    host: Arc<dyn Host>,
    /// 密钥来源（U7 注入接缝；默认 [`KeyringKeys`]）
    keys: Arc<dyn KeySource>,
    /// 分片云端存取覆盖（U13 stub；`None` = FeishuClient 生产路径）
    chunk_store: Option<Arc<dyn ChunkStore>>,
    /// <app_config_dir>/transfer
    work_dir: PathBuf,
    pub quota: Arc<Quota>,
    tasks: Mutex<HashMap<String, Arc<TaskHandle>>>,
    /// 待确认下载会话：handle → PendingDownload（仅内存，随链接新鲜度窗口过期）
    sessions: Mutex<HashMap<String, PendingDownload>>,
    /// 已消费指纹缓存（consumed.json，30 天，§9.4 重放抑制第三道闸）
    consumed: Mutex<ConsumedCache>,
    /// 终态事件暂存（done/failed/cancelled）——前端注册监听前事件可能已发出，注册后轮询一次补偿
    final_events: Mutex<HashMap<String, serde_json::Value>>,
}

#[derive(Clone)]
pub struct UploadRequest {
    pub path: String,
    pub webhook_url: String,
    pub webhook_secret: String,
}

#[derive(Serialize)]
pub struct UploadAccepted {
    pub task_id: String,
    pub name: String,
    pub size: u64,
}

/// CLI 同步发送结果（M1）
pub struct SendOutcome {
    /// 取回链接（已写入卡片并发到群）
    pub link: String,
}

/// 一次待确认的下载（会话）
#[derive(Clone)]
pub struct PendingDownload {
    pub handle: String,
    pub fingerprint: String,
    pub env: Envelope,
    /// 解密 payload 所用的密钥（hex）——创建会话时已选定
    pub key_hex: String,
    pub app_id: String,
    pub app_secret: String,
    pub download_dir: String,
    pub created_at: i64,
    /// 会话有效期截止（= payload.ts + 新鲜度窗口，与链接有效期同源）
    pub expires_at: i64,
    /// 领取后分配的任务标识 / 取消旗标
    pub task_id: Option<String>,
    pub cancel: Option<Arc<AtomicBool>>,
}

#[derive(Default, Serialize, Deserialize)]
struct ConsumedCache {
    /// 指纹 → {at, path}
    items: HashMap<String, ConsumedItem>,
}

#[derive(Serialize, Deserialize, Clone)]
struct ConsumedItem {
    at: i64,
    #[serde(default)]
    path: String,
}

/// claim_session 结果
pub enum ClaimResult {
    /// 开始下载（首次或失败后重试）
    Start(PendingDownload),
    /// 已成功下载过 → 幂等返回结果（§10.1/§9.4）
    AlreadyDone { path: String },
}

#[derive(Debug)]
pub enum ClaimError {
    Busy,
    Expired,
    NotFound,
}

/// 会话创建结果（确认页渲染所需）
pub struct SessionView {
    pub handle: String,
    pub name: String,
    pub size: u64,
    pub created_at: i64,
    /// 会话有效期截止（确认页「剩余有效期」倒计时同源）
    pub expires_at: i64,
    pub download_dir: String,
}

/// 会话有效期 = payload.ts + 新鲜度窗口：确认页倒计时必须与「链接 N 分钟内有效」同源，
/// 否则页面倒数与群消息承诺的时长不一致（且过期后再按「开始下载」必然失败）。
fn session_expires_at(ts: i64) -> i64 {
    ts + crypto::FRESHNESS_WINDOW_SECS
}

/// payload 校验结果（无副作用）
#[derive(Debug)]
pub struct Evaluated {
    pub fingerprint: String,
    pub key_hex: String,
}

impl Engine {
    pub fn open(work_dir: PathBuf, host: Arc<dyn Host>) -> Result<Self, String> {
        Self::open_with(work_dir, host, Arc::new(KeyringKeys), None)
    }

    /// 显式注入密钥来源与分片存取（测试 / CI 用；生产路径走 [`Engine::open`]）
    pub fn open_with(
        work_dir: PathBuf,
        host: Arc<dyn Host>,
        keys: Arc<dyn KeySource>,
        chunk_store: Option<Arc<dyn ChunkStore>>,
    ) -> Result<Self, String> {
        std::fs::create_dir_all(&work_dir).map_err(|e| format!("创建 transfer 目录失败: {e}"))?;
        let quota = Quota::open(&work_dir)?;
        let consumed = Self::load_consumed(&work_dir);
        FingerprintLock::sweep_orphans(&work_dir);
        Ok(Self {
            host,
            keys,
            chunk_store,
            work_dir,
            quota: Arc::new(quota),
            tasks: Mutex::new(HashMap::new()),
            sessions: Mutex::new(HashMap::new()),
                    consumed: Mutex::new(consumed),
            final_events: Mutex::new(HashMap::new()),
        })
    }

    /// 记录任务终态事件（供前端注册后轮询补偿）
    pub(crate) fn record_final(&self, task_id: &str, v: serde_json::Value) {
        self.final_events
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .insert(task_id.to_string(), v);
    }

    /// 取走终态事件（有则返回）
    pub fn take_final_event(&self, task_id: &str) -> Option<serde_json::Value> {
        self.final_events
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .remove(task_id)
    }

    fn load_consumed(dir: &PathBuf) -> ConsumedCache {
        std::fs::read_to_string(dir.join("consumed.json"))
            .ok()
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default()
    }

    /* ===================== 上传（§8） ===================== */

    /// 发起上传：校验通过后立即返回 task_id，实际传输在后台线程执行。
    pub fn start_upload(
        self: &Arc<Self>,
        req: UploadRequest,
        app_id: String,
        app_secret: String,
    ) -> Result<UploadAccepted, String> {
        if app_id.is_empty() || app_secret.is_empty() {
            return Err("未配置飞书应用凭证（App ID / App Secret）".into());
        }
        if req.webhook_url.is_empty() {
            return Err("未指定发送目标机器人".into());
        }
        let path = PathBuf::from(&req.path);
        let meta = std::fs::metadata(&path).map_err(|e| format!("无法读取文件: {e}"))?;
        if !meta.is_file() {
            return Err("路径不是一个文件".into());
        }
        let size = meta.len();
        if size > MAX_FILE_SIZE {
            return Err(format!("单文件不能超过 {} MB（D18）", MAX_FILE_SIZE / 1024 / 1024));
        }
        let name = path
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_else(|| "file".into());

        // 主密钥必须已配置（无 K 无法加密）；经 KeySource（M0a 接缝），生产 = keyring
        let key_hex = self.keys.current()?
            .ok_or_else(|| "尚未配置传输密钥：请先在设置 · 文件传输中生成或导入".to_string())?;
        let key = crypto::key_from_hex(&key_hex)?;

        let task_id = crypto::hex(&crypto::random_bytes(8));
        let cancel = Arc::new(AtomicBool::new(false));
        self.tasks
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .insert(task_id.clone(), Arc::new(TaskHandle { cancel: cancel.clone(), dir: "out", fp_lock: Mutex::new(None) }));

        let host = self.host.clone();
        let quota = self.quota.clone();
        let work_dir = self.work_dir.clone();
        let engine = self.clone();
        let task_id2 = task_id.clone();
        std::thread::Builder::new()
            .name(format!("qn-upload-{task_id}"))
            .spawn(move || match upload_inner(host.as_ref(), &quota, &work_dir, &task_id2, &cancel, &req, &key, &app_id, &app_secret, size) {
                Ok(link) => {
                    emit_progress(host.as_ref(), &task_id2, "out", "done", size, size, size, size, None, Some(&link), None);
                    engine.record_final(&task_id2, serde_json::json!({"state":"done","dir":"out","link":link}));
                }
                Err(msg) => {
                    let state = if cancel.load(Ordering::Relaxed) { "cancelled" } else { "failed" };
                    emit_progress(host.as_ref(), &task_id2, "out", state, 0, size, 0, 0, Some(&msg), None, None);
                    engine.record_final(&task_id2, serde_json::json!({"state":state,"dir":"out","error":msg}));
                }
            })
            .map_err(|e| format!("启动上传线程失败: {e}"))?;

        Ok(UploadAccepted { task_id, name, size })
    }

    /// CLI 同步发送（M1）：加密上传 + 发卡片，**当前线程**完成（无后台任务、无进度事件）。
    /// 返回取回链接与本机接收告警（M0c：本机服务未运行不构成失败）。
    /// 额度计入与 APP 共享的同一 `quota.json`（同一 work_dir）。
    pub fn send_file_sync(&self, req: &UploadRequest) -> Result<SendOutcome, String> {
        let app_id = self.app_id_of()?;
        let app_secret = self.app_secret_of()?;
        if app_id.is_empty() || app_secret.is_empty() {
            return Err("未配置飞书应用凭证（App ID / App Secret）".into());
        }
        if req.webhook_url.is_empty() {
            return Err("未指定发送目标机器人".into());
        }
        let path = PathBuf::from(&req.path);
        let meta = std::fs::metadata(&path).map_err(|e| format!("无法读取文件: {e}"))?;
        if !meta.is_file() {
            return Err("路径不是一个文件".into());
        }
        let size = meta.len();
        if size > MAX_FILE_SIZE {
            return Err(format!("单文件不能超过 {} MB（D18）", MAX_FILE_SIZE / 1024 / 1024));
        }

        let key_hex = self.keys.current()?
            .ok_or_else(|| "尚未配置传输密钥：请先在 APP 设置或 CLI 中生成/导入".to_string())?;
        let key = crypto::key_from_hex(&key_hex)?;

        let task_id = crypto::hex(&crypto::random_bytes(8));
        let cancel = Arc::new(AtomicBool::new(false));
        let link = upload_inner(
            self.host.as_ref(),
            &self.quota,
            &self.work_dir,
            &task_id,
            &cancel,
            req,
            &key,
            &app_id,
            &app_secret,
            size,
        )?;
        Ok(SendOutcome { link })
    }

    /// 取消任务（cooperative：在下一个片边界生效）
    pub fn cancel(&self, task_id: &str) -> Result<(), String> {
        let tasks = self.tasks.lock().unwrap_or_else(|p| p.into_inner());
        match tasks.get(task_id) {
            Some(h) => {
                h.cancel.store(true, Ordering::Relaxed);
                Ok(())
            }
            None => Err("任务不存在或已结束".into()),
        }
    }

    /// 退出协议：全部任务标记取消（TransferController::cancel_all）
    pub fn cancel_all(&self) {
        for h in self.tasks.lock().unwrap_or_else(|p| p.into_inner()).values() {
            h.cancel.store(true, Ordering::Relaxed);
        }
    }

    /// 正在进行中的任务数（TransferActivityProvider::snapshot）
    pub fn cancellable_count(&self) -> usize {
        self.tasks.lock().unwrap_or_else(|p| p.into_inner()).len()
    }

    /* ===================== payload 校验与会话（§7.5 / §10.1） ===================== */

    /// 从链接（…/dl?t=payload）或裸 payload 提取并校验（无副作用）：
    /// K 解密 → v/AAD → 元数据自洽 → ts 新鲜度（§7.5 第 1–4 步）
    pub fn evaluate_payload(&self, input: &str) -> Result<Evaluated, String> {
        let payload = extract_payload(input)?;
        if payload.len() > 20 * 1024 {
            return Err("载荷超出 20 KB 上限".into());
        }
        // 按 kid 选择 K：先试 current，再试 previous（§12.3；30 天保留期由 keyring 层强制）
        let cur = self.keys.current()?;
        let prev = self.keys.previous()?;
        let mut last_err: Option<String> = None;
        for hex_key in [cur.as_deref(), prev.as_deref()].into_iter().flatten() {
            let key = crypto::key_from_hex(hex_key)?;
            match open_payload(&key, &payload) {
                Ok(env) => {
                    if crypto::fingerprint(&key) != env.kid {
                        return Err("payload 与密钥标识不一致（kid 校验失败）".into());
                    }
                    validate_metadata(&env.meta).map_err(|e| format!("载荷不合法：{e}"))?;
                    if !crypto::is_fresh(env.meta.ts, crypto::now_unix()) {
                        return Err(format!(
                            "链接已过期（ts 新鲜度窗口 {} 分钟）",
                            crypto::FRESHNESS_WINDOW_MINUTES
                        ));
                    }
                    return Ok(Evaluated {
                        fingerprint: crypto::payload_fingerprint(&payload),
                        key_hex: hex_key.to_string(),
                    });
                }
                Err(e) => last_err = Some(e),
            }
        }
        Err(last_err.unwrap_or_else(|| {
            if cur.is_none() {
                "本机尚未配置传输密钥：请在配置 · 文件传输中生成或导入".into()
            } else {
                "密钥不匹配：确认两端已配对同一 K，或链接来自已轮换旧密钥".into()
            }
        }))
    }

    /// 创建一次性确认会话（handle，仅内存，活到链接新鲜度窗口结束）
    pub fn create_session(&self, payload: &str, ev: Evaluated) -> Result<SessionView, String> {
        let payload = extract_payload(payload)?;
        // 解出 meta 供确认页展示（key 已在 evaluate 时验证）
        let key = crypto::key_from_hex(&ev.key_hex)?;
        let env: Envelope = open_payload(&key, &payload)?;
        // 解析下载目录（配置优先，默认系统下载目录；D5）
        let download_dir = self.host.resolve_download_dir()?;
        let handle = crypto::hex(&crypto::random_bytes(16));
        let now = crypto::now_unix();
        let expires_at = session_expires_at(env.meta.ts);
        // GC 过期会话（按链接有效期，与确认页倒计时同源）
        self.sessions
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .retain(|_, s| now <= s.expires_at);
        let pending = PendingDownload {
            handle: handle.clone(),
            fingerprint: ev.fingerprint,
            env,
            key_hex: ev.key_hex,
            app_id: self.app_id_of()?,
            app_secret: self.app_secret_of()?,
            download_dir,
            created_at: now,
            expires_at,
            task_id: None,
            cancel: None,
        };
        let view = SessionView {
            handle: handle.clone(),
            name: pending.env.meta.name.clone(),
            size: pending.env.meta.size,
            created_at: now,
            expires_at,
            download_dir: pending.download_dir.clone(),
        };
        self.sessions
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .insert(handle, pending);
        Ok(view)
    }

    /// 原子领取会话（§10.1 校验顺序 ③ 的单消费部分）
    pub fn claim_session(&self, handle: &str) -> Result<ClaimResult, ClaimError> {
        let now = crypto::now_unix();
        let pending = {
            let mut sessions = self.sessions.lock().unwrap_or_else(|p| p.into_inner());
            let p = sessions.get(handle).cloned().ok_or(ClaimError::NotFound)?;
            if now > p.expires_at {
                sessions.remove(handle);
                return Err(ClaimError::Expired);
            }
            p
        };
        // 幂等：已成功 → 直接回结果（§10.1/§9.4）
        {
            let consumed = self.consumed.lock().unwrap_or_else(|p| p.into_inner());
            if let Some(item) = consumed.items.get(&pending.fingerprint) {
                return Ok(ClaimResult::AlreadyDone { path: item.path.clone() });
            }
        }
        // 单消费锁（M0b：跨进程 OS advisory lock，D24）：被持有 → Busy（U17 409）
        let fp_lock = match FingerprintLock::try_acquire(&self.work_dir, &pending.fingerprint) {
            Ok(g) => g,
            Err(_) => return Err(ClaimError::Busy),
        }
        .ok_or(ClaimError::Busy)?;
        // handle 用后即焚
        self.sessions.lock().unwrap_or_else(|p| p.into_inner()).remove(handle);

        let task_id = crypto::hex(&crypto::random_bytes(8));
        let cancel = Arc::new(AtomicBool::new(false));
        self.tasks
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .insert(
                task_id.clone(),
                Arc::new(TaskHandle {
                    cancel: cancel.clone(),
                    dir: "in",
                    fp_lock: Mutex::new(Some(fp_lock)),
                }),
            );
        Ok(ClaimResult::Start(PendingDownload::with_task(pending, task_id, cancel)))
    }

    /* ===================== 下载执行（§9.3/§9.4/§7.5 第 5–7 步） ===================== */

    /// 同步执行下载（确认页 worker 线程 / 前端命令线程共用）。
    /// 返回最终落盘路径。进度经 transfer://progress 推送。
    pub fn run_download_sync(&self, pending: &PendingDownload) -> Result<String, String> {
        let host = self.host.clone();
        let task_id = pending.task_id.clone().unwrap_or_else(|| crypto::hex(&crypto::random_bytes(8)));
        // 指纹锁在本函数存活期内持有（成功/失败/取消都在此释放；失败后重试可重新 claim）
        let _fp_guard = {
            let tasks = self.tasks.lock().unwrap_or_else(|p| p.into_inner());
            match tasks.get(&task_id) {
                Some(h) => h.fp_lock.lock().unwrap_or_else(|p| p.into_inner()).take(),
                None => None, // 未走 claim 的调用方（如 CLI recv）无锁可持
            }
        };
        let cancel = pending.cancel.clone().unwrap_or_else(|| Arc::new(AtomicBool::new(false)));
        let env = &pending.env;
        let meta = &env.meta;
        let total = meta.size;
        let t0 = Instant::now();

        let result = (|| -> Result<String, String> {
            // 分片来源：测试注入的 stub 优先（U13）；生产走 FeishuClient
            let store: Arc<dyn ChunkStore> = match &self.chunk_store {
                Some(s) => s.clone(),
                None => Arc::new(FeishuChunks {
                    client: FeishuClient::new(&pending.app_id, &pending.app_secret, self.quota.clone())?,
                }),
            };
            let dek = base64::engine::general_purpose::URL_SAFE_NO_PAD
                .decode(&env.dek)
                .map_err(|_| "DEK 解码失败".to_string())?;
            if dek.len() != 32 {
                return Err("DEK 长度不合法".into());
            }
            let tid = hex_decode_16(&env.tid)?;
            // 密钥在 evaluate_payload 时已校验；此处保留引用以防后续需要（如重签）
            let _key = crypto::key_from_hex(&pending.key_hex)?;

            // 落盘准备（§9.3）
            let dir = PathBuf::from(&pending.download_dir);
            std::fs::create_dir_all(&dir).map_err(|e| format!("创建下载目录失败: {e}"))?;
            let safe_name = crypto::sanitize_filename(&meta.name);
            let fp8 = &pending.fingerprint[..8.min(pending.fingerprint.len())];
            let part_path = dir.join(format!("{safe_name}.{fp8}.part"));
            let sidecar_path = dir.join(format!("{safe_name}.{fp8}.json"));

            // 断点续传：读侧片记录（§9.4）
            #[derive(Default, Serialize, Deserialize)]
            struct Sidecar {
                name: String,
                size: u64,
                sha256: String,
                completed: Vec<u32>,
            }
            let mut sidecar: Sidecar = std::fs::read_to_string(&sidecar_path)
                .ok()
                .and_then(|s| serde_json::from_str(&s).ok())
                .filter(|s: &Sidecar| s.size == total && s.sha256 == meta.sha256)
                .unwrap_or(Sidecar {
                    name: safe_name.clone(),
                    size: total,
                    sha256: meta.sha256.clone(),
                    completed: Vec::new(),
                });

            let mut part = OpenOptions::new()
                .create(true)
                .read(true)
                .write(true)
                .truncate(false)
                .open(&part_path)
                .map_err(|e| format!("打开 .part 失败: {e}"))?;
            part.set_len(total).map_err(|e| format!("预分配 .part 失败: {e}"))?;

            // 逐片下载 → 解密 → 按 off 写入（§7.5 第 5 步）
            let mut done: u64 = sidecar.completed.iter().filter_map(|&n| meta.chunks.get(n as usize).map(|c| c.size)).sum();
            for chunk in &meta.chunks {
                if cancel.load(Ordering::Relaxed) {
                    return Err("已取消".into());
                }
                if sidecar.completed.contains(&chunk.n) {
                    continue;
                }
                let sealed = store.fetch(&chunk.t)?;
                let nonce = base64::engine::general_purpose::URL_SAFE_NO_PAD
                    .decode(&chunk.nonce)
                    .map_err(|_| "分片 nonce 解码失败".to_string())?;
                if nonce.len() != NONCE_LEN {
                    return Err("分片 nonce 长度不合法".into());
                }
                let plain = open_chunk(&dek, &nonce, &tid, chunk.n, &sealed)
                    .map_err(|e| format!("第 {} 片解密失败：{e}", chunk.n))?;
                if plain.len() as u64 != chunk.size {
                    return Err(format!("第 {} 片明文长度不合法", chunk.n));
                }
                part.seek(SeekFrom::Start(chunk.off)).map_err(|e| format!("seek 失败: {e}"))?;
                part.write_all(&plain).map_err(|e| format!("写 .part 失败: {e}"))?;
                drop(plain);
                sidecar.completed.push(chunk.n);
                // 持久化片记录（断点续传）
                if let Ok(j) = serde_json::to_string(&sidecar) {
                    let _ = std::fs::write(&sidecar_path, j);
                }
                done += chunk.size;
                let rate = (done as f64 / t0.elapsed().as_secs_f64().max(0.001)) as u64;
                emit_progress(host.as_ref(), &task_id, "in", "running", done, total, done, total, None, None, Some(rate));
            }
            // 完整落盘后强制刷盘
            part.sync_all().map_err(|e| format!("刷盘失败: {e}"))?;
            drop(part);

            // SHA-256 校验（§7.5 第 6 步）
            let mut f = std::fs::File::open(&part_path).map_err(|e| format!("打开 .part 失败: {e}"))?;
            let actual = hex_sha256_stream(&mut f)?;
            drop(f);
            if actual != meta.sha256 {
                let _ = std::fs::remove_file(&part_path);
                let _ = std::fs::remove_file(&sidecar_path);
                return Err("合并后 SHA-256 不匹配：分片缺失/错序/损坏".into());
            }

            // 原子 rename + 重名追加序号（§9.3）
            let final_path = dedup_path(&dir, &safe_name);
            std::fs::rename(&part_path, &final_path).map_err(|e| format!("落盘失败: {e}"))?;
            let _ = std::fs::remove_file(&sidecar_path);

            // 下载即删（D11）：校验通过后删除云端全部分片；失败进重试队列
            let mut failed_deletes: Vec<String> = Vec::new();
            for chunk in &meta.chunks {
                if store.delete(&chunk.t).is_err() {
                    failed_deletes.push(chunk.t.clone());
                }
            }
            if !failed_deletes.is_empty() {
                self.queue_pending_deletes(&failed_deletes);
            }

            Ok(final_path.to_string_lossy().to_string())
        })();

        match result {
            Ok(final_path) => {
                self.mark_consumed(&pending.fingerprint, &final_path);
                self.tasks.lock().unwrap_or_else(|p| p.into_inner()).remove(&task_id);
                emit_progress(host.as_ref(), &task_id, "in", "done", total, total, total, total, None, None, None);
                self.record_final(&task_id, serde_json::json!({"state":"done","dir":"in","final_path":final_path}));
                // 浏览器取回无应用内记录，单独广播终态，前端据此补写历史
                host.downloaded(serde_json::json!({
                        "task_id": task_id,
                        "name": meta.name,
                        "size": total,
                        "final_path": final_path,
                        "fingerprint": pending.fingerprint,
                }));
                Ok(final_path)
            }
            Err(msg) => {
                let state = if cancel.load(Ordering::Relaxed) { "cancelled" } else { "failed" };
                // 指纹锁随 _fp_guard drop 释放（失败后重试路径：重新 GET /dl → 新会话 → 断点续传）
                // 取消：清理 .part 与侧车；失败：保留以便断点续传（§9.4/U13）
                if state == "cancelled" {
                    self.cleanup_part(pending);
                }
                self.tasks.lock().unwrap_or_else(|p| p.into_inner()).remove(&task_id);
                emit_progress(host.as_ref(), &task_id, "in", state, 0, total, 0, 0, Some(&msg), None, None);
                self.record_final(&task_id, serde_json::json!({"state":state,"dir":"in","error":msg}));
                Err(msg)
            }
        }
    }

    fn cleanup_part(&self, pending: &PendingDownload) {
        let dir = PathBuf::from(&pending.download_dir);
        let safe_name = crypto::sanitize_filename(&pending.env.meta.name);
        let fp8 = &pending.fingerprint[..8.min(pending.fingerprint.len())];
        let _ = std::fs::remove_file(dir.join(format!("{safe_name}.{fp8}.part")));
        let _ = std::fs::remove_file(dir.join(format!("{safe_name}.{fp8}.json")));
    }

    /* ===================== 已消费缓存（§9.4） ===================== */

    pub fn consumed_at(&self, fingerprint: &str) -> Option<i64> {
        let c = self.consumed.lock().unwrap_or_else(|p| p.into_inner());
        c.items.get(fingerprint).map(|i| i.at)
    }

    pub fn consumed_path_of(&self, fingerprint: &str) -> Option<String> {
        let c = self.consumed.lock().unwrap_or_else(|p| p.into_inner());
        c.items.get(fingerprint).map(|i| i.path.clone())
    }

    fn mark_consumed(&self, fingerprint: &str, path: &str) {
        let now = crypto::now_unix();
        // 锁内重读磁盘（另一进程的已消费记录不得丢失，D24 ②）→ 合并 → 原子写
        let merged = super::statefile::update::<ConsumedCache, _>(
            &self.work_dir.join("consumed.json"),
            |disk| {
                let mut c: ConsumedCache = disk
                    .and_then(|v| serde_json::from_value(v.clone()).ok())
                    .unwrap_or_default();
                c.items.retain(|_, i| now - i.at < 30 * 24 * 3600);
                c.items.insert(
                    fingerprint.to_string(),
                    ConsumedItem { at: now, path: path.to_string() },
                );
                Ok(c)
            },
        );
        if merged.is_err() {
            log::warn!("consumed.json 落盘失败（内存缓存已更新）");
        }
        let mut c = self.consumed.lock().unwrap_or_else(|p| p.into_inner());
        c.items.retain(|_, i| now - i.at < 30 * 24 * 3600);
        c.items.insert(fingerprint.to_string(), ConsumedItem { at: now, path: path.to_string() });
    }

    /* ===================== 删除重试队列（§6.4） ===================== */

    fn queue_pending_deletes(&self, tokens: &[String]) {
        let path = self.work_dir.join("pending_deletes.json");
        let tokens = tokens.to_vec();
        // 锁内重读合并 + 原子写（D24；另一进程的队列条目不得丢失）
        let r = super::statefile::update::<Vec<String>, _>(&path, |disk| {
            let mut list: Vec<String> = disk
                .and_then(|v| serde_json::from_value(v.clone()).ok())
                .unwrap_or_default();
            for t in &tokens {
                if !list.contains(t) {
                    list.push(t.clone());
                }
            }
            Ok(list)
        });
        if r.is_err() {
            log::warn!("pending_deletes.json 落盘失败");
        }
        log::warn!("删除分片失败 {} 个，已进入重试队列（定期清理兜底）", tokens.len());
    }

    /// 启动时重试删除队列（删除请求已发出但响应丢失 → 404 即成功）。
    ///
    /// D32：锁纪律（清理方案 §5.1）——绝不在持状态文件锁期间发网络请求。三段式：
    /// ① 锁内读出全部待删 token（文件不动，**重试中途崩溃零丢失**）；
    /// ② 锁外逐个重试；
    /// ③ 锁内重读并按结果改写——成功者移除；失败者与期间其他进程经
    ///    `queue_pending_deletes` 新入队的 token 一律保留（合并去重，非整表覆盖）。
    /// 语义与 D29 相反：本队列是**在线路径**，失败必须留队列重试，勿混。
    pub fn retry_pending_deletes(&self, app_id: &str, app_secret: &str) {
        if app_id.is_empty() {
            return;
        }
        let path = self.work_dir.join("pending_deletes.json");
        // ① 锁内读出（读完立即释放锁，文件内容原样保留）
        let list: Vec<String> = {
            let (_guard, disk) = match super::statefile::with_exclusive(&path) {
                Ok(x) => x,
                Err(e) => {
                    log::warn!("pending_deletes.json 加锁失败，跳过本轮重试: {e}");
                    return;
                }
            };
            disk.and_then(|v| serde_json::from_value(v).ok()).unwrap_or_default()
        };
        if list.is_empty() {
            return;
        }
        // ② 锁外逐个重试（404 已在 delete_file 内归一为 Ok）
        let client = match FeishuClient::new(app_id, app_secret, self.quota.clone()) {
            Ok(c) => c,
            Err(_) => return,
        };
        let mut succeeded: std::collections::HashSet<String> = Default::default();
        let mut failed: std::collections::HashSet<String> = Default::default();
        for t in &list {
            match client.delete_file(t) {
                Ok(()) => {
                    succeeded.insert(t.clone());
                }
                Err(_) => {
                    failed.insert(t.clone());
                }
            }
        }
        // ③ 锁内重读并按结果改写（成功者移除；失败者与期间新入队者保留）
        let r = super::statefile::update::<Vec<String>, _>(&path, |disk| {
            let mut cur: Vec<String> = disk
                .and_then(|v| serde_json::from_value(v.clone()).ok())
                .unwrap_or_default();
            cur.retain(|t| !succeeded.contains(t));
            for t in &failed {
                if !cur.contains(t) {
                    cur.push(t.clone());
                }
            }
            Ok(cur)
        });
        if r.is_err() {
            log::warn!("pending_deletes.json 改写失败（失败条目仍在原文件中，下次启动重试）");
        }
        if !failed.is_empty() {
            log::info!("删除重试后仍有 {} 个未清除", failed.len());
        }
    }

    /* ===================== 凭证来源（qingniao.json） ===================== */

    fn app_id_of(&self) -> Result<String, String> {
        let path = self.host.config_dir()?.join("qingniao.json");
        let cfg: serde_json::Value = std::fs::read_to_string(&path)
            .ok()
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or(serde_json::Value::Null);
        Ok(cfg.get("app_id").and_then(|v| v.as_str()).unwrap_or("").to_string())
    }

    fn app_secret_of(&self) -> Result<String, String> {
        let path = self.host.config_dir()?.join("qingniao.json");
        let cfg: serde_json::Value = std::fs::read_to_string(&path)
            .ok()
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or(serde_json::Value::Null);
        Ok(cfg.get("app_secret").and_then(|v| v.as_str()).unwrap_or("").to_string())
    }

    pub fn work_dir(&self) -> &std::path::Path {
        &self.work_dir
    }
}

impl PendingDownload {
    fn with_task(mut self, task_id: String, cancel: Arc<AtomicBool>) -> PendingDownload {
        self.task_id = Some(task_id);
        self.cancel = Some(cancel);
        self
    }
}

/* ===================== 下载目录 / 工具（D5） ===================== */

/// 从链接（…/dl?t=payload）、被包装/百分号编码的链接，或裸 payload 提取载荷（§9.2 兜底入口）
///
/// 飞书客户端可能把 href 包成跳转链接（`…?url=http%3A%2F%2F127.0.0.1%3A9876%2Fdl%3Ft%3D…`），
/// 这类形态先解一层百分号编码再找 `t=`；payload 本体是 base64url（无 `%`），解码对裸载荷是恒等变换。
pub fn extract_payload(input: &str) -> Result<String, String> {
    let s = input.trim();
    if s.is_empty() {
        return Err("载荷为空".into());
    }
    let decoded = percent_decode(s);
    if let Some(pos) = decoded.find("t=") {
        let raw = &decoded[pos + 2..];
        let end = raw.find('&').unwrap_or(raw.len());
        let t = &raw[..end];
        if t.is_empty() {
            return Err("链接缺少载荷".into());
        }
        return Ok(t.to_string());
    }
    // 没有 t= 参数：只有「整串就是个 base64url 载荷」才按裸载荷处理；
    // 粘成消息正文或别的链接时给出可操作提示，而不是含糊的「载荷不合法」
    if !s.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_') {
        return Err("没找到取回载荷：请右键取回链接选「复制链接地址」，或只粘 t= 后面那段；消息正文里不含链接".into());
    }
    Ok(s.to_string())
}

/// 仅解 `%XX`（不把 `+` 当空格：base64url 里 `+` 无特殊语义）
fn percent_decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%' && i + 2 < b.len() {
            if let (Some(hi), Some(lo)) =
                ((b[i + 1] as char).to_digit(16), (b[i + 2] as char).to_digit(16))
            {
                out.push((hi * 16 + lo) as u8);
                i += 3;
                continue;
            }
        }
        out.push(b[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).to_string()
}

fn dedup_path(dir: &std::path::Path, name: &str) -> PathBuf {
    let candidate = dir.join(name);
    if !candidate.exists() {
        return candidate;
    }
    // 重名追加序号：name → name_1 → name_2 …（§9.3）
    let stem = std::path::Path::new(name).file_stem().map(|s| s.to_string_lossy().to_string()).unwrap_or_else(|| name.to_string());
    let ext = std::path::Path::new(name).extension().map(|s| s.to_string_lossy().to_string());
    for n in 1u32.. {
        let candidate = match &ext {
            Some(e) => dir.join(format!("{stem}_{n}.{e}")),
            None => dir.join(format!("{stem}_{n}")),
        };
        if !candidate.exists() {
            return candidate;
        }
    }
    unreachable!("序号穷尽不可能")
}

fn hex_decode_16(s: &str) -> Result<Vec<u8>, String> {
    if s.len() != 32 {
        return Err("tid 长度不合法".into());
    }
    (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&s[i..i + 2], 16).map_err(|_| "tid 非法 hex".to_string()))
        .collect()
}

/* ===================== 上传执行体（§8） ===================== */

#[allow(clippy::too_many_arguments)]
fn upload_inner(
    host: &dyn Host,
    quota: &Arc<Quota>,
    work_dir: &std::path::Path,
    task_id: &str,
    cancel: &Arc<AtomicBool>,
    req: &UploadRequest,
    key: &[u8],
    app_id: &str,
    app_secret: &str,
    total: u64,
) -> Result<String, String> {
    let t0 = Instant::now();
    let client = FeishuClient::new(app_id, app_secret, quota.clone())?;

    // 1. 确保云目录（§6.3）
    let root = client.root_folder().map_err(feishu_msg)?;
    let month_dir = client.ensure_transfer_dir(&root).map_err(feishu_msg)?;

    // 2. 生成 DEK / tid（§7.2）
    let mut dek = vec![0u8; 32];
    rand::rngs::OsRng.fill_bytes(&mut dek);
    let tid = crypto::random_bytes(16);

    // 3. 逐片加密上传（§8.2，串行）
    let chunks_total = total.div_ceil(CHUNK_SIZE as u64) as u32;
    let mut chunk_metas: Vec<ChunkMeta> = Vec::with_capacity(chunks_total as usize);
    let mut file = std::fs::File::open(&req.path).map_err(|e| format!("打开文件失败: {e}"))?;
    let mut done: u64 = 0;
    for n in 0..chunks_total {
        if cancel.load(Ordering::Relaxed) {
            return Err("已取消".into());
        }
        let chunk_plain = read_exact_chunk(&mut file, CHUNK_SIZE)?;
        let plain_len = chunk_plain.len() as u64;
        let nonce = crypto::random_bytes(NONCE_LEN);
        let sealed = seal_chunk(&dek, &nonce, &tid, n, &chunk_plain)?;
        drop(chunk_plain); // 尽早释放明文内存
        let cloud_name = format!("q2-{}", crypto::hex(&crypto::random_bytes(8)));
        let token = client.upload_chunk(&month_dir, &cloud_name, sealed).map_err(feishu_msg)?;
        chunk_metas.push(ChunkMeta {
            t: token,
            n,
            off: done,
            size: plain_len,
            nonce: base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(&nonce),
        });
        done += plain_len;
        let rate = (done as f64 / t0.elapsed().as_secs_f64().max(0.001)) as u64;
        emit_progress(host, task_id, "out", "running", done, total, done, total, None, None, Some(rate));
    }

    // 4. 组装 metadata + payload（§8.3）
    // 登记点物料（D27）：全部 chunk token 在 meta 被移入 Envelope 前取出
    let chunk_tokens: Vec<String> = chunk_metas.iter().map(|c| c.t.clone()).collect();
    let mut file2 = std::fs::File::open(&req.path).map_err(|e| format!("打开文件失败: {e}"))?;
    let sha256 = hex_sha256_stream(&mut file2)?;
    let display_name = std::path::Path::new(&req.path)
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_else(|| "file".into());
    let meta = Metadata {
        name: display_name.clone(),
        mime: String::new(),
        size: total,
        sha256,
        ts: crypto::now_unix(),
        chunks: chunk_metas,
    };
    // 卡片 footer 展示的发送时间与新鲜度窗口同源（§8.3）
    let sent_ts = meta.ts;
    let env = Envelope {
        v: crypto::PROTO_VERSION,
        kid: crypto::fingerprint(key),
        dek: base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(&dek),
        tid: crypto::hex(&tid),
        meta,
    };
    let link = send_tail(host, req, key, &env, &display_name, total, sent_ts, &chunk_tokens)?;

    // 登记点（D27/P1-1，必须在 upload_inner——APP 线程路径与 CLI send_file_sync 的共用实现）：
    // 发送成功 → 登记本次全部分片在链接窗口 + 宽限期后删除
    crate::transfer::cleanup::schedule_deletes(
        work_dir,
        &chunk_tokens,
        sent_ts + crypto::FRESHNESS_WINDOW_SECS + crate::transfer::cleanup::CLEANUP_GRACE_SECS,
    );
    Ok(link)
}

/// upload_inner 的尾部（组装 payload → 链接 → 发卡片）。独立出来以便失败路径
/// 统一记日志（D29）：分片已全部上传后任何一步失败（seal_payload、webhook 非
/// 2xx、webhook 网络错误），已上传分片成为孤儿——不删、不登记，只 log::warn!
/// 等待每周 48 h 扫描兜底。webhook 响应读超时但消息实际已发出的歧义情形同此。
fn send_tail(
    host: &dyn Host,
    req: &UploadRequest,
    key: &[u8],
    env: &Envelope,
    display_name: &str,
    total: u64,
    sent_ts: i64,
    chunk_tokens: &[String],
) -> Result<String, String> {
    let result = (|| -> Result<String, String> {
        let payload = seal_payload(key, env)?;

        // 5. 链接端口 = 配置端口（D23/M0c）：接收端据 configured_port 拨号，
        //    与发送端本机服务的运行时状态无关；发送与接收完全解耦（2026-09-19 用户拍板：不告警不拒绝）
        let link = build_transfer_link(host.configured_port(), &payload);

        // 6. 仅链接形式发群（D3/D4）；webhook 不计入月度额度
        let card = build_transfer_card(display_name, total, &link, sent_ts);
        let (status, body) = send_webhook_json(&req.webhook_url, &card, Some(&req.webhook_secret))?;
        if !(200..300).contains(&status) {
            return Err(format!("取回链接发送失败：HTTP {status} {body}"));
        }
        Ok(link)
    })();
    match result {
        Ok(link) => Ok(link),
        Err(e) => {
            let prefix: String = chunk_tokens.first().map(|t| t.chars().take(8).collect()).unwrap_or_default();
            log::warn!(
                "发送失败，本次遗留 {} 片（{}…），等待定期清理",
                chunk_tokens.len(),
                prefix
            );
            Err(e)
        }
    }
}

/// 取回链接组装（D23：端口 = 配置值 configured_port，非本机运行时端口）
fn build_transfer_link(configured_port: u16, payload: &str) -> String {
    format!("http://127.0.0.1:{configured_port}/dl?t={payload}")
}

fn feishu_msg(e: crate::transfer::feishu::FeishuError) -> String {
    e.to_string()
}

fn read_exact_chunk(file: &mut std::fs::File, cap: usize) -> Result<Vec<u8>, String> {
    let mut buf = vec![0u8; cap];
    let mut filled = 0usize;
    while filled < cap {
        match file.read(&mut buf[filled..]) {
            Ok(0) => break,
            Ok(n) => filled += n,
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(format!("读取文件失败: {e}")),
        }
    }
    buf.truncate(filled);
    if buf.is_empty() {
        return Err("文件内容为空".into());
    }
    Ok(buf)
}

fn hex_sha256_stream(file: &mut std::fs::File) -> Result<String, String> {
    file.seek(SeekFrom::Start(0)).map_err(|e| format!("seek 失败: {e}"))?;
    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; 1024 * 1024];
    loop {
        let n = file.read(&mut buf).map_err(|e| format!("读取文件失败: {e}"))?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    file.seek(SeekFrom::Start(0)).map_err(|e| format!("seek 失败: {e}"))?;
    Ok(crypto::hex(&hasher.finalize()))
}

/* ===================== 进度 emit ===================== */

#[allow(clippy::too_many_arguments)]
pub(crate) fn emit_progress(
    host: &dyn Host,
    task_id: &str,
    dir: &str,
    state: &str,
    bytes_done: u64,
    bytes_total: u64,
    chunk_index: u64,
    chunk_total: u64,
    error: Option<&str>,
    link: Option<&str>,
    rate_bps: Option<u64>,
) {
    host.progress(serde_json::json!({
            "task_id": task_id,
            "dir": dir,
            "state": state,
            "bytes_done": bytes_done,
            "bytes_total": bytes_total,
            "chunk_index": chunk_index,
            "chunk_total": chunk_total,
            "error": error,
            "link": link,
            "rate_bps": rate_bps,
    }));
}

#[allow(dead_code)]
pub(crate) fn write_part_chunk(part: &mut std::fs::File, off: u64, data: &[u8]) -> Result<(), String> {
    part.seek(SeekFrom::Start(off)).map_err(|e| format!("seek 失败: {e}"))?;
    part.write_all(data).map_err(|e| format!("写 .part 失败: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    #[test]
    fn extract_payload_accepts_link_and_bare_payload() {
        let payload = "abc-_123XYZ";
        assert_eq!(extract_payload(&format!("http://127.0.0.1:9876/dl?t={payload}")).unwrap(), payload);
        assert_eq!(extract_payload(payload).unwrap(), payload);
        // 前后空白与「整条粘贴」中的多余参数都不影响取 t
        assert_eq!(extract_payload(&format!("  http://127.0.0.1:9876/dl?t={payload}&x=1  ")).unwrap(), payload);
    }

    #[test]
    fn extract_payload_decodes_wrapped_link() {
        // 飞书客户端把 href 包成跳转链接（url= 参数里的链接被百分号编码）
        let payload = "abc-_123XYZ";
        let wrapped = format!(
            "https://applink.feishu.cn/client/web_url/open?url=http%3A%2F%2F127.0.0.1%3A9876%2Fdl%3Ft%3D{payload}&mode=window"
        );
        assert_eq!(extract_payload(&wrapped).unwrap(), payload);
    }

    #[test]
    fn extract_payload_rejects_empty_and_missing_payload() {
        assert!(extract_payload("   ").is_err());
        assert!(extract_payload("http://127.0.0.1:9876/dl?t=").is_err());
        // 粘成消息正文 / 别的链接 → 明确提示，而不是拿去当载荷解密
        let msg = "kimi-code-win32-x64.zip 点击取回（53.8 MB · 链接 10 分钟内有效）";
        assert!(extract_payload(msg).is_err());
        assert!(extract_payload("https://xxx.feishu.cn/file/abc").is_err());
    }

    #[test]
    fn session_expiry_tracks_freshness_window() {
        let ts = crypto::now_unix();
        let expires = session_expires_at(ts);
        assert_eq!(expires - ts, crypto::FRESHNESS_WINDOW_SECS);
        // 链接新鲜度窗口内创建 → 会话未过期；窗口外 → 已过期
        assert!(crypto::now_unix() <= expires);
        assert!(crypto::now_unix() > session_expires_at(crypto::now_unix() - crypto::FRESHNESS_WINDOW_SECS - 1));
    }

    /* ===== M0c：链接端口与接收告警（D23） ===== */

    /// ① 服务未运行 / 端口不可得时，仍产出 configured_port 的 URL
    #[test]
    fn m0c_link_uses_configured_port_even_without_service() {
        let host = FixedHost::new(PathBuf::from("/tmp/qn-m0c"))
            .with_configured_port(12345)
            .with_service_port(None);
        assert_eq!(host.configured_port(), 12345);
        assert_eq!(
            build_transfer_link(host.configured_port(), "abc-_123XYZ"),
            "http://127.0.0.1:12345/dl?t=abc-_123XYZ"
        );
        // 缺省 = DEFAULT_LOCAL_PORT
        assert_eq!(FixedHost::new(PathBuf::from("/tmp/qn-m0c")).configured_port(), DEFAULT_LOCAL_PORT);
    }

    /// ② （2026-09-19 用户拍板）服务未运行**不告警不拒绝**——发送与接收完全解耦。
    /// 原「降为告警」设计随 M0c ②修订取消；本测试钉住「不告警」的口径。
    #[test]
    fn m0c_service_down_does_not_warn() {
        let down = FixedHost::new(PathBuf::from("/tmp/qn-m0c")).with_service_port(None);
        // Host 的 service_bound_port 仅剩查询语义；无任何告警产生路径
        assert_eq!(down.service_bound_port(), None);
    }

    /* ===== D33：sweep_orphans 只清指纹锁 ===== */

    fn sweep_temp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("qn-sweep-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// statefile 的 4 个常驻锁专用文件（`<name>.json.lock`）不得被当孤儿扫掉
    /// （否则会在「他人 open 之后、try_lock 之前」的窗口拆锁、破坏互斥）；
    /// 名字不合规的 `.lock` 同样保留；无人持锁的孤儿指纹锁（`<64hex>.lock`）仍删。
    #[test]
    fn sweep_orphans_spares_state_lock_files_and_illegal_names() {
        let dir = sweep_temp_dir("shape");
        let hex64 = "a".repeat(64);
        let kept: Vec<String> = vec![
            // statefile 锁专用文件（D33）
            "quota.json.lock".into(),
            "consumed.json.lock".into(),
            "pending_deletes.json.lock".into(),
            "cleanup.json.lock".into(),
            // 名字不合规：太短 / 混大写 / 非 hex
            "abc.lock".into(),
            "deadbeef.lock".into(),
            format!("{}B{}.lock", "a".repeat(31), "a".repeat(32)),
            format!("{}.lock", "g".repeat(64)),
        ];
        for n in &kept {
            std::fs::write(dir.join(n), b"").unwrap();
        }
        let orphan = dir.join(format!("{hex64}.lock"));
        std::fs::write(&orphan, b"").unwrap();

        FingerprintLock::sweep_orphans(&dir);

        assert!(!orphan.exists(), "无人持锁的孤儿指纹锁必须被删除");
        for n in &kept {
            assert!(dir.join(n).exists(), "{n} 不得被扫掉");
        }
        // 数据文件本体不受影响（不匹配 *.lock 的名字一律跳过）
        std::fs::write(dir.join("quota.json"), b"{}").unwrap();
        FingerprintLock::sweep_orphans(&dir);
        assert!(dir.join("quota.json").exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 既有语义保持：正被持有的指纹锁不删（flock 在持锁者 fd 上，sweep 锁不上）；
    /// 释放后即成为孤儿，可被下一轮清掉。
    #[test]
    fn sweep_orphans_spares_held_fingerprint_lock_until_released() {
        let dir = sweep_temp_dir("held");
        let hex64 = "b".repeat(64);
        let guard = FingerprintLock::try_acquire(&dir, &hex64).unwrap().expect("获取指纹锁");
        let path = dir.join(format!("{hex64}.lock"));
        FingerprintLock::sweep_orphans(&dir);
        assert!(path.exists(), "正被持有的指纹锁不得被扫掉");
        drop(guard);
        FingerprintLock::sweep_orphans(&dir);
        assert!(!path.exists(), "释放后即成孤儿，应被删除");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 纯函数：路径形状判定逐类钉死
    #[test]
    fn fingerprint_lock_path_shape() {
        let ok = |stem: String| is_fingerprint_lock_path(Path::new(&format!("/w/{stem}.lock")));
        assert!(ok("c".repeat(64)));
        assert!(ok("0123456789abcdef".repeat(4)));
        assert!(!ok("A".repeat(64)), "大写 hex 不是指纹锁");
        assert!(!ok("g".repeat(64)), "非 hex 字符不是指纹锁");
        assert!(!ok("a".repeat(63)), "长度必须恰为 64");
        assert!(!ok("a".repeat(65)), "长度必须恰为 64");
        assert!(!ok("quota.json".into()), "statefile 锁专用文件不是指纹锁");
        assert!(!is_fingerprint_lock_path(Path::new("/w/abc.lock")));
        assert!(!is_fingerprint_lock_path(Path::new("/w/orphan.lock")));
        assert!(!is_fingerprint_lock_path(Path::new("/w/noext")));
    }
}
