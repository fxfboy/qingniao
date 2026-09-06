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
use crate::transfer::feishu::{build_transfer_post, send_webhook_json, FeishuClient};
use crate::transfer::quota::Quota;
use base64::Engine as _;
use rand::RngCore;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::fs::OpenOptions;
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;
use tauri::{AppHandle, Emitter, Manager};

/// 单任务控制块
pub struct TaskHandle {
    pub cancel: Arc<AtomicBool>,
    #[allow(dead_code)]
    pub dir: &'static str, // "out" | "in"
}

pub struct Engine {
    app: AppHandle,
    /// <app_config_dir>/transfer
    work_dir: PathBuf,
    pub quota: Arc<Quota>,
    tasks: Mutex<HashMap<String, Arc<TaskHandle>>>,
    /// 待确认下载会话：handle → PendingDownload（仅内存，10 min 过期）
    sessions: Mutex<HashMap<String, PendingDownload>>,
    /// per-指纹状态（§9.4 单消费）：Downloading / Done
    fp_state: Mutex<HashMap<String, FpState>>,
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
    /// 领取后分配的任务标识 / 取消旗标
    pub task_id: Option<String>,
    pub cancel: Option<Arc<AtomicBool>>,
}

/// 指纹级下载状态
#[derive(Clone)]
enum FpState {
    Downloading,
    Done { path: String },
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
    pub download_dir: String,
}

/// payload 校验结果（无副作用）
pub struct Evaluated {
    pub fingerprint: String,
    pub key_hex: String,
}

impl Engine {
    pub fn open(work_dir: PathBuf, app: AppHandle) -> Result<Self, String> {
        std::fs::create_dir_all(&work_dir).map_err(|e| format!("创建 transfer 目录失败: {e}"))?;
        let quota = Quota::open(&work_dir)?;
        let consumed = Self::load_consumed(&work_dir);
        Ok(Self {
            app,
            work_dir,
            quota: Arc::new(quota),
            tasks: Mutex::new(HashMap::new()),
            sessions: Mutex::new(HashMap::new()),
            fp_state: Mutex::new(HashMap::new()),
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

        // 主密钥必须已配置（无 K 无法加密）
        let key_hex = keyring_store::get_current()?
            .ok_or_else(|| "尚未配置传输密钥：请先在设置 · 文件传输中生成或导入".to_string())?;
        let key = crypto::key_from_hex(&key_hex)?;

        let task_id = crypto::hex(&crypto::random_bytes(8));
        let cancel = Arc::new(AtomicBool::new(false));
        self.tasks
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .insert(task_id.clone(), Arc::new(TaskHandle { cancel: cancel.clone(), dir: "out" }));

        let app = self.app.clone();
        let engine = self.clone();
        let task_id2 = task_id.clone();
        std::thread::Builder::new()
            .name(format!("qn-upload-{task_id}"))
            .spawn(move || match upload_inner(&app, &task_id2, &cancel, &req, &key, &app_id, &app_secret, size) {
                Ok(link) => {
                    emit_progress(&app, &task_id2, "out", "done", size, size, size, size, None, Some(&link), None);
                    engine.record_final(&task_id2, serde_json::json!({"state":"done","dir":"out","link":link}));
                }
                Err(msg) => {
                    let state = if cancel.load(Ordering::Relaxed) { "cancelled" } else { "failed" };
                    emit_progress(&app, &task_id2, "out", state, 0, size, 0, 0, Some(&msg), None, None);
                    engine.record_final(&task_id2, serde_json::json!({"state":state,"dir":"out","error":msg}));
                }
            })
            .map_err(|e| format!("启动上传线程失败: {e}"))?;

        Ok(UploadAccepted { task_id, name, size })
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
        let cur = keyring_store::get_current()?;
        let prev = keyring_store::get_previous()?;
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
                        return Err("链接已过期（ts 新鲜度窗口 30 分钟）".into());
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

    /// 创建一次性确认会话（handle 10 min，仅内存）
    pub fn create_session(&self, payload: &str, ev: Evaluated) -> Result<SessionView, String> {
        let payload = extract_payload(payload)?;
        // 解出 meta 供确认页展示（key 已在 evaluate 时验证）
        let key = crypto::key_from_hex(&ev.key_hex)?;
        let env: Envelope = open_payload(&key, &payload)?;
        // 解析下载目录（配置优先，默认系统下载目录；D5）
        let download_dir = download_dir_of(&self.app)?;
        let handle = crypto::hex(&crypto::random_bytes(16));
        let now = crypto::now_unix();
        // GC 过期会话
        self.sessions
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .retain(|_, s| now - s.created_at <= crate::local_server::HANDLE_TTL_SECS);
        let pending = PendingDownload {
            handle: handle.clone(),
            fingerprint: ev.fingerprint,
            env,
            key_hex: ev.key_hex,
            app_id: self.app_id_of()?,
            app_secret: self.app_secret_of()?,
            download_dir,
            created_at: now,
            task_id: None,
            cancel: None,
        };
        let view = SessionView {
            handle: handle.clone(),
            name: pending.env.meta.name.clone(),
            size: pending.env.meta.size,
            created_at: now,
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
            if now - p.created_at > crate::local_server::HANDLE_TTL_SECS {
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
        // 单消费锁：同一指纹仅一个任务进入「消费中」
        {
            let mut states = self.fp_state.lock().unwrap_or_else(|p| p.into_inner());
            match states.get(&pending.fingerprint) {
                Some(FpState::Downloading) => return Err(ClaimError::Busy),
                Some(FpState::Done { path, .. }) => {
                    return Ok(ClaimResult::AlreadyDone { path: path.clone() });
                }
                None => {}
            }
            states.insert(pending.fingerprint.clone(), FpState::Downloading);
        }
        // handle 用后即焚
        self.sessions.lock().unwrap_or_else(|p| p.into_inner()).remove(handle);

        let task_id = crypto::hex(&crypto::random_bytes(8));
        let cancel = Arc::new(AtomicBool::new(false));
        self.tasks
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .insert(task_id.clone(), Arc::new(TaskHandle { cancel: cancel.clone(), dir: "in" }));
        Ok(ClaimResult::Start(PendingDownload::with_task(pending, task_id, cancel)))
    }

    /* ===================== 下载执行（§9.3/§9.4/§7.5 第 5–7 步） ===================== */

    /// 同步执行下载（确认页 worker 线程 / 前端命令线程共用）。
    /// 返回最终落盘路径。进度经 transfer://progress 推送。
    pub fn run_download_sync(&self, pending: &PendingDownload) -> Result<String, String> {
        let app = self.app.clone();
        let task_id = pending.task_id.clone().unwrap_or_else(|| crypto::hex(&crypto::random_bytes(8)));
        let cancel = pending.cancel.clone().unwrap_or_else(|| Arc::new(AtomicBool::new(false)));
        let env = &pending.env;
        let meta = &env.meta;
        let total = meta.size;
        let t0 = Instant::now();

        let result = (|| -> Result<String, String> {
            let client = FeishuClient::new(&pending.app_id, &pending.app_secret, self.quota.clone())?;
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
                let sealed = client.download_chunk(&chunk.t).map_err(|e| {
                    if e.http_status == 404 {
                        "分片已被删除（可能已被取回或清理），请对方重新上传".to_string()
                    } else if e.http_status == 403 {
                        "无下载权限：请检查应用权限配置".to_string()
                    } else {
                        e.to_string()
                    }
                })?;
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
                emit_progress(&app, &task_id, "in", "running", done, total, done, total, None, None, Some(rate));
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
                if client.delete_file(&chunk.t).is_err() {
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
                emit_progress(&app, &task_id, "in", "done", total, total, total, total, None, None, None);
                self.record_final(&task_id, serde_json::json!({"state":"done","dir":"in","final_path":final_path}));
                // 浏览器取回无应用内记录，单独广播终态，前端据此补写历史
                let _ = app.emit(
                    "transfer://downloaded",
                    serde_json::json!({
                        "task_id": task_id,
                        "name": meta.name,
                        "size": total,
                        "final_path": final_path,
                        "fingerprint": pending.fingerprint,
                    }),
                );
                Ok(final_path)
            }
            Err(msg) => {
                let state = if cancel.load(Ordering::Relaxed) { "cancelled" } else { "failed" };
                // 释放指纹锁（失败后重试路径：重新 GET /dl → 新会话 → 断点续传）
                self.fp_state
                    .lock()
                    .unwrap_or_else(|p| p.into_inner())
                    .remove(&pending.fingerprint);
                // 取消：清理 .part 与侧车；失败：保留以便断点续传（§9.4/U13）
                if state == "cancelled" {
                    self.cleanup_part(pending);
                }
                self.tasks.lock().unwrap_or_else(|p| p.into_inner()).remove(&task_id);
                emit_progress(&app, &task_id, "in", state, 0, total, 0, 0, Some(&msg), None, None);
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
        {
            let mut c = self.consumed.lock().unwrap_or_else(|p| p.into_inner());
            // 30 天保留：顺带清理过期项
            c.items.retain(|_, i| now - i.at < 30 * 24 * 3600);
            c.items.insert(fingerprint.to_string(), ConsumedItem { at: now, path: path.to_string() });
            if let Ok(j) = serde_json::to_string_pretty(&*c) {
                let tmp = self.work_dir.join("consumed.json.tmp");
                if std::fs::write(&tmp, j).is_ok() {
                    let _ = std::fs::rename(&tmp, self.work_dir.join("consumed.json"));
                }
            }
        }
        // 指纹状态 → Done（幂等 200 的依据）
        let mut states = self.fp_state.lock().unwrap_or_else(|p| p.into_inner());
        states.insert(fingerprint.to_string(), FpState::Done { path: path.to_string() });
    }

    /* ===================== 删除重试队列（§6.4） ===================== */

    fn queue_pending_deletes(&self, tokens: &[String]) {
        let path = self.work_dir.join("pending_deletes.json");
        let mut list: Vec<String> = std::fs::read_to_string(&path)
            .ok()
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default();
        for t in tokens {
            if !list.contains(t) {
                list.push(t.clone());
            }
        }
        if let Ok(j) = serde_json::to_string(&list) {
            let _ = std::fs::write(&path, j);
        }
        log::warn!("删除分片失败 {} 个，已进入重试队列（定期清理兜底）", tokens.len());
    }

    /// 启动时重试删除队列（删除请求已发出但响应丢失 → 404 即成功）
    pub fn retry_pending_deletes(&self, app_id: &str, app_secret: &str) {
        let path = self.work_dir.join("pending_deletes.json");
        let list: Vec<String> = std::fs::read_to_string(&path)
            .ok()
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default();
        if list.is_empty() || app_id.is_empty() {
            return;
        }
        let client = match FeishuClient::new(app_id, app_secret, self.quota.clone()) {
            Ok(c) => c,
            Err(_) => return,
        };
        let mut remain = Vec::new();
        for t in list {
            match client.delete_file(&t) {
                Ok(()) => {}
                Err(e) if e.http_status == 404 => {}
                Err(_) => remain.push(t),
            }
        }
        let _ = std::fs::write(&path, serde_json::to_string(&remain).unwrap_or_else(|_| "[]".into()));
    }

    /* ===================== 凭证来源（qingniao.json） ===================== */

    fn app_id_of(&self) -> Result<String, String> {
        let path = self
            .app
            .path()
            .app_config_dir()
            .map_err(|e| format!("无法定位配置目录: {e}"))?
            .join("qingniao.json");
        let cfg: serde_json::Value = std::fs::read_to_string(&path)
            .ok()
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or(serde_json::Value::Null);
        Ok(cfg.get("app_id").and_then(|v| v.as_str()).unwrap_or("").to_string())
    }

    fn app_secret_of(&self) -> Result<String, String> {
        let path = self
            .app
            .path()
            .app_config_dir()
            .map_err(|e| format!("无法定位配置目录: {e}"))?
            .join("qingniao.json");
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

/// 下载目录 = 配置项，默认系统下载目录（目录不存在自动创建）
pub fn download_dir_of(app: &AppHandle) -> Result<String, String> {
    let cfg_path = app
        .path()
        .app_config_dir()
        .map_err(|e| format!("无法定位配置目录: {e}"))?
        .join("qingniao.json");
    let configured: Option<String> = std::fs::read_to_string(&cfg_path)
        .ok()
        .and_then(|s| serde_json::from_str::<serde_json::Value>(&s).ok())
        .and_then(|v| v.pointer("/transfer/download_dir").and_then(|d| d.as_str()).map(String::from));
    let dir = match configured {
        Some(d) if !d.is_empty() => PathBuf::from(d),
        _ => app.path().download_dir().map_err(|e| format!("无法定位系统下载目录: {e}"))?,
    };
    std::fs::create_dir_all(&dir).map_err(|e| format!("创建下载目录失败: {e}"))?;
    Ok(dir.to_string_lossy().to_string())
}

/// 从链接（…/dl?t=payload）或裸 payload 提取载荷（§9.2 兜底入口两种形态）
pub fn extract_payload(input: &str) -> Result<String, String> {
    let s = input.trim();
    if let Some(pos) = s.find("t=") {
        let raw = &s[pos + 2..];
        let end = raw.find('&').unwrap_or(raw.len());
        let t = &raw[..end];
        if t.is_empty() {
            return Err("链接缺少载荷".into());
        }
        return Ok(t.to_string());
    }
    if s.is_empty() {
        return Err("载荷为空".into());
    }
    Ok(s.to_string())
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
    app: &tauri::AppHandle,
    task_id: &str,
    cancel: &Arc<AtomicBool>,
    req: &UploadRequest,
    key: &[u8],
    app_id: &str,
    app_secret: &str,
    total: u64,
) -> Result<String, String> {
    let t0 = Instant::now();
    let client = FeishuClient::new(app_id, app_secret, engine_quota(app)?)?;

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
        emit_progress(app, task_id, "out", "running", done, total, done, total, None, None, Some(rate));
    }

    // 4. 组装 metadata + payload（§8.3）
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
    let env = Envelope {
        v: crypto::PROTO_VERSION,
        kid: crypto::fingerprint(key),
        dek: base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(&dek),
        tid: crypto::hex(&tid),
        meta,
    };
    let payload = seal_payload(key, &env)?;

    // 5. 链接只用实际绑定端口（P0-3：禁止写死 configured_port）
    let bound_port = super::service_bound_port(app)
        .ok_or_else(|| "本地服务未运行，无法生成取回链接：请先在设置中启动本地服务".to_string())?;
    let link = format!("http://127.0.0.1:{bound_port}/dl?t={payload}");

    // 6. 仅链接形式发群（D3/D4）；webhook 不计入月度额度
    let post = build_transfer_post(&display_name, total, &link);
    let (status, body) = send_webhook_json(&req.webhook_url, &post, Some(&req.webhook_secret))?;
    if !(200..300).contains(&status) {
        return Err(format!("取回链接发送失败：HTTP {status} {body}"));
    }
    Ok(link)
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
    app: &tauri::AppHandle,
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
    let _ = app.emit(
        "transfer://progress",
        serde_json::json!({
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
        }),
    );
}

/// 从 AppState 取全局 Quota（FeishuClient 需要计数器）
fn engine_quota(app: &tauri::AppHandle) -> Result<Arc<Quota>, String> {
    super::engine_quota_of(app)
}

#[allow(dead_code)]
pub(crate) fn write_part_chunk(part: &mut std::fs::File, off: u64, data: &[u8]) -> Result<(), String> {
    part.seek(SeekFrom::Start(off)).map_err(|e| format!("seek 失败: {e}"))?;
    part.write_all(data).map_err(|e| format!("写 .part 失败: {e}"))
}
