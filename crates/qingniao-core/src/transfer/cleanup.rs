//! 云端清理（文件传输云端清理方案 v0.3，D26–D30）：补齐协议 §6.4 承诺但未落地的两条策略。
//!
//! - **发送端定时清理**（D27/D28）：发送成功后把本次全部分片的 `{t, at}` 登记进
//!   状态文件 `cleanup.json`（`at = ts + 10 min + 30 min` 宽限期），由到期门控的
//!   flush 在 APP ticker / CLI 命令开始时真正删除——把「发了没人取回」的驻留时间
//!   从「最长一周」压到「约 40 分钟」；
//! - **每周 48 h 孤儿扫描**（D30）：每 7 天一次，列举 `青鸟传输` 下**全部** `YYYY-MM`
//!   目录，删除 `created_time > 48 h` 的对象（判据用云端时间，天然免疫双方时钟偏差）。
//!
//! 失败处置（D29）：定时清理删除失败 = 从计划移除后**不重试、不回填**，只
//! `log::warn!`；残留交给每周扫描兜底。`pending_deletes.json`（下载即删的重试
//! 队列）语义**原样不动**——在线路径重试、离线路径只记日志，这条不对称是刻意的。
//!
//! **锁纪律（§5.1，最重要）**：绝不在持状态文件锁期间发网络请求。flush 与扫描
//! 都是「先认领、后执行」：锁内读出到期条目并从文件移除 / 置位 `last_sweep_at`，
//! 锁外再发 delete / list。这同时给出天然去重——同一 token 只可能被一个进程认领一次。

use crate::transfer::feishu::FeishuClient;
use crate::transfer::quota::Quota;
use crate::transfer::statefile;
use serde::{Deserialize, Serialize};
use std::path::Path;
use std::sync::Arc;

/// 宽限期（D28）：链接新鲜度窗口（10 min）外再加 30 min，`delete_after = ts + 600 + 1800`。
/// 调节钮：需同时扛「分钟级时钟偏差 + <57 KB/s 网速」时可调到 45 min（单常量改动）。
pub const CLEANUP_GRACE_SECS: i64 = 30 * 60;

/// 孤儿判定阈值（D30）：云端 `created_time` 距今 > 48 h 视为孤儿。
pub const ORPHAN_AGE_SECS: i64 = 48 * 3600;

/// 扫描门控周期（D30）：`last_sweep_at` 距今 ≥ 7 天才认领新一轮扫描。
pub const SWEEP_INTERVAL_SECS: i64 = 7 * 24 * 3600;

/// 状态文件名（`<work_dir>/cleanup.json`，与 quota.json 等同目录）
const STATE_FILE: &str = "cleanup.json";

const CLEANUP_VERSION: u8 = 1;

/* ===================== 状态文件（§5.1） ===================== */

#[derive(Serialize, Deserialize, Clone)]
struct ScheduledEntry {
    /// 分片 file_token
    t: String,
    /// 到期时刻（unix 秒 = ts + 600 + 1800）
    at: i64,
}

#[derive(Serialize, Deserialize)]
struct CleanupState {
    #[serde(default = "default_version")]
    version: u8,
    #[serde(default)]
    scheduled: Vec<ScheduledEntry>,
    /// 上次**认领**扫描的时刻；缺省视为「从未扫过」→ 首次运行即扫一次
    #[serde(default)]
    last_sweep_at: Option<i64>,
}

fn default_version() -> u8 {
    CLEANUP_VERSION
}

impl Default for CleanupState {
    fn default() -> Self {
        Self { version: CLEANUP_VERSION, scheduled: Vec::new(), last_sweep_at: None }
    }
}

impl CleanupState {
    /// 解析磁盘值：JSON 损坏或 version 不匹配 → **按空处理并覆盖**（§5.1；
    /// 与 quota.rs 的 unwrap_or_default 同口径——该文件可丢，丢了只是少一轮清理）
    fn parse(disk: Option<&serde_json::Value>) -> Self {
        let Some(v) = disk else { return Self::default() };
        match serde_json::from_value::<CleanupState>(v.clone()) {
            Ok(st) if st.version == CLEANUP_VERSION => st,
            _ => Self::default(),
        }
    }
}

/* ===================== 云端存取接缝 ===================== */

/// 云端目录中的一个对象（供 [`CleanupStore`] 列举返回）
#[derive(Clone, Debug)]
pub struct CleanupEntry {
    pub token: String,
    pub name: String,
    /// created_time 换算成 unix 秒（毫秒护栏；缺失/不可解析 = 0，扫描时跳过）
    pub created_time_secs: i64,
    /// 是否为目录（D30：月份目录识别须 `type == "folder"`，同名文件不得当目录列举）
    pub is_dir: bool,
}

/// 云端目录的列举与删除。生产实现走 [`FeishuClient`]（见 [`run_due_feishu`]）；
/// 测试注入 stub（沿用 engine.rs `ChunkStore` / U13 的注入范式）。
pub trait CleanupStore: Send + Sync {
    fn root_folder(&self) -> Result<String, String>;
    /// 列举目录（翻页取全由实现负责）
    fn list_dir(&self, folder_token: &str) -> Result<Vec<CleanupEntry>, String>;
    /// 删除云端对象（404 = 已删除，实现层视为成功）
    fn delete(&self, file_token: &str) -> Result<(), String>;
}

/// 一次 run_due 的汇总（供调用方输出一行）
#[derive(Default, Debug)]
pub struct CleanupReport {
    /// 定时清理：删除成功数
    pub scheduled_flushed: usize,
    /// 定时清理：删除失败数（已从计划移除，仅记日志，等扫描兜底）
    pub flush_failed: usize,
    /// 扫描：删除孤儿数
    pub orphan_deleted: usize,
    /// 扫描：删除失败数（仅记日志，下次扫描自然重试）
    pub orphan_failed: usize,
    /// 本轮是否认领并执行了扫描（7 天门控）
    pub swept: bool,
    /// 扫描/状态文件层面的异常说明（不影响调用方流程）
    pub note: Option<String>,
}

/* ===================== 登记（发送成功后） ===================== */

/// 发送成功后登记本次全部分片的待删计划（D27）：幂等追加——同 token 已存在则跳过
/// （参照 `queue_pending_deletes` 的 contains 判定），返回本次新增条数。
/// 落盘失败只 `log::warn!`（该批分片退化为等每周扫描兜底）。
pub fn schedule_deletes(work_dir: &Path, tokens: &[String], delete_after: i64) -> usize {
    let path = work_dir.join(STATE_FILE);
    let mut added = 0usize;
    let r = write_state(&path, |st| {
        for t in tokens {
            if !st.scheduled.iter().any(|e| &e.t == t) {
                st.scheduled.push(ScheduledEntry { t: t.clone(), at: delete_after });
                added += 1;
            }
        }
    });
    if let Err(e) = r {
        log::warn!("cleanup: cleanup.json 写入失败，本批 {} 片未登记计划（每周扫描兜底）: {e}", tokens.len());
    }
    added
}

/* ===================== 到期 flush + 每周扫描 ===================== */

/// 纯逻辑入口（可测）：认领到期的待删计划并执行删除；到期时认领并执行每周扫描。
///
/// 锁纪律（§5.1）：两次状态文件访问都只在锁内做「读 + 改 + 原子写」，网络请求全部在锁外。
pub fn run_due(store: &dyn CleanupStore, work_dir: &Path, now: i64) -> CleanupReport {
    let mut report = CleanupReport::default();
    let path = work_dir.join(STATE_FILE);

    // ① 认领到期的定时删除条目（锁内移除 → 锁外删除；天然去重：同 token 只被认领一次）
    let due = match claim_due(&path, now) {
        Ok(d) => d,
        Err(e) => {
            log::warn!("cleanup: cleanup.json 读写失败，跳过本轮定时清理: {e}");
            report.note = Some(format!("cleanup.json 读写失败: {e}"));
            Vec::new()
        }
    };
    let due_total = due.len();
    for entry in &due {
        match store.delete(&entry.t) {
            Ok(()) => report.scheduled_flushed += 1,
            Err(e) => {
                // D29：已认领即移除，不重试不回填；瞬态失败 = 最坏等 7 天扫描兜底
                report.flush_failed += 1;
                log::warn!(
                    "cleanup: 定时删除 {}… 失败，不再重试（每周扫描兜底）: {e}",
                    short_token(&entry.t)
                );
            }
        }
    }

    // ② 7 天门控的扫描认领（D30）：锁内「读门控 + 置位 last_sweep_at」一次性完成；
    //    扫描失败不回滚该时间戳（D29 口径：只记日志，下次门控到期再扫）
    let mut last_sweep: Option<i64> = None;
    match claim_sweep(&path, now, &mut last_sweep) {
        Err(e) => {
            log::warn!("cleanup: cleanup.json 读写失败，跳过本轮扫描: {e}");
            report.note = report.note.take().or_else(|| Some(format!("cleanup.json 读写失败: {e}")));
        }
        Ok(false) => {}
        Ok(true) => {
            report.swept = true;
            let (deleted, failed, note) = sweep(store, now);
            report.orphan_deleted = deleted;
            report.orphan_failed = failed;
            if let Some(n) = note {
                log::warn!("cleanup: 扫描未完成: {n}");
                report.note = report.note.take().or(Some(n));
            }
        }
    }

    // 汇总一行（§5.5）
    let days = last_sweep.map(|t| (now - t).max(0) / 86_400);
    log::info!(
        "清理: 定时删除 {}/{}，扫描删除 {}（失败 {}）（上次扫描 {}）",
        report.scheduled_flushed,
        due_total,
        report.orphan_deleted,
        report.orphan_failed,
        match days {
            Some(d) => format!("{d} 天前"),
            None => "从未".to_string(),
        }
    );
    report
}

/// 锁内认领到期的待删条目：读出 → 从文件移除 → 原子写回。无到期条目时不写盘。
fn claim_due(path: &Path, now: i64) -> Result<Vec<ScheduledEntry>, String> {
    let (guard, disk) = statefile::with_exclusive(path)?;
    let mut st = CleanupState::parse(disk.as_ref());
    let due: Vec<ScheduledEntry> = st.scheduled.iter().filter(|e| e.at <= now).cloned().collect();
    if due.is_empty() {
        drop(guard);
        return Ok(due);
    }
    st.scheduled.retain(|e| e.at > now);
    write_under_guard(path, &st)?;
    drop(guard);
    Ok(due)
}

/// 锁内认领扫描：`last_sweep_at` 缺省或距今 ≥ [`SWEEP_INTERVAL_SECS`] 时置位 `now`。
/// 返回 `Ok(false)` = 未到门控（不写盘）；`Ok(true)` = 已认领本轮。
fn claim_sweep(path: &Path, now: i64, prev: &mut Option<i64>) -> Result<bool, String> {
    let (guard, disk) = statefile::with_exclusive(path)?;
    let mut st = CleanupState::parse(disk.as_ref());
    *prev = st.last_sweep_at;
    let due = st.last_sweep_at.map_or(true, |t| now.saturating_sub(t) >= SWEEP_INTERVAL_SECS);
    if due {
        st.last_sweep_at = Some(now);
        write_under_guard(path, &st)?;
    }
    drop(guard);
    Ok(due)
}

/// 执行一轮孤儿扫描（D30）：root meta → 列根找「青鸟传输」→ 列「青鸟传输」取
/// 全部月份目录 → 逐个列举并删除 `created_time > 48 h` 的文件。
/// 固定开销 = 3 + N 次 list（N = 月份目录数）。单条失败只 warn、不中断整体。
fn sweep(store: &dyn CleanupStore, now: i64) -> (usize, usize, Option<String>) {
    // 列根找「青鸟传输」（只列不建：扫描路径不得复用 ensure_transfer_dir）
    let root = match store.root_folder() {
        Ok(r) => r,
        Err(e) => return (0, 0, Some(format!("获取根目录失败: {e}"))),
    };
    let root_entries = match store.list_dir(&root) {
        Ok(v) => v,
        Err(e) => return (0, 0, Some(format!("列举根目录失败: {e}"))),
    };
    let Some(transfer) = root_entries.iter().find(|e| e.is_dir && e.name == "青鸟传输").map(|e| e.token.clone()) else {
        return (0, 0, Some("根目录下未找到「青鸟传输」".into()));
    };
    let transfer_entries = match store.list_dir(&transfer) {
        Ok(v) => v,
        Err(e) => return (0, 0, Some(format!("列举「青鸟传输」失败: {e}"))),
    };
    // 月份目录识别：type == folder 且名字匹配 ^\d{4}-\d{2}$（全部月份，D30；只列不建）
    let months: Vec<&CleanupEntry> = transfer_entries
        .iter()
        .filter(|e| e.is_dir && is_month_dir(&e.name))
        .collect();
    let mut deleted = 0usize;
    let mut failed = 0usize;
    for month in months {
        let entries = match store.list_dir(&month.token) {
            Ok(v) => v,
            Err(e) => {
                log::warn!("cleanup: 列举月份目录 {} 失败，跳过该目录: {e}", month.name);
                failed += 1;
                continue;
            }
        };
        for e in entries {
            // created_time <= 0（缺失/不可解析）不可判龄，跳过——宁漏删勿误删；
            // 嵌套目录不删（月份目录内只应存在分片文件）
            if e.is_dir || e.created_time_secs <= 0 {
                continue;
            }
            // 判据用云端 created_time（天然免疫收发双方时钟偏差）；边界：严格大于
            if now - e.created_time_secs > ORPHAN_AGE_SECS {
                match store.delete(&e.token) {
                    Ok(()) => {
                        deleted += 1;
                        log::info!("cleanup: 扫描删除孤儿 {}…（{}）", short_token(&e.token), month.name);
                    }
                    Err(err) => {
                        failed += 1;
                        log::warn!("cleanup: 扫描删除 {}… 失败（下次扫描自然重试）: {err}", short_token(&e.token));
                    }
                }
            }
        }
    }
    (deleted, failed, None)
}

/* ===================== 生产实现 ===================== */

/// 生产包装：复用调用方已有的 `Arc<Quota>` 建 [`FeishuClient`] 后调用 [`run_due`]。
/// `app_id` 为空直接 no-op 并记一条 debug（对齐 `retry_pending_deletes` 的早退）。
pub fn run_due_feishu(
    work_dir: &Path,
    app_id: &str,
    app_secret: &str,
    quota: Arc<Quota>,
    now: i64,
) -> CleanupReport {
    if app_id.is_empty() {
        log::debug!("cleanup: 未配置飞书应用凭证，跳过云端清理");
        return CleanupReport { note: Some("未配置飞书应用凭证".into()), ..Default::default() };
    }
    let client = match FeishuClient::new(app_id, app_secret, quota) {
        Ok(c) => c,
        Err(e) => {
            log::warn!("cleanup: 创建飞书客户端失败，跳过本轮清理: {e}");
            return CleanupReport { note: Some(format!("创建飞书客户端失败: {e}")), ..Default::default() };
        }
    };
    run_due(&FeishuCleanupStore { client }, work_dir, now)
}

/// [`CleanupStore`] 的生产实现：薄包装 [`FeishuClient`]，错误统一转字符串。
struct FeishuCleanupStore {
    client: FeishuClient,
}

impl CleanupStore for FeishuCleanupStore {
    fn root_folder(&self) -> Result<String, String> {
        self.client.root_folder().map_err(|e| e.to_string())
    }
    fn list_dir(&self, folder_token: &str) -> Result<Vec<CleanupEntry>, String> {
        self.client
            .list_folder(folder_token)
            .map(|files| {
                files
                    .into_iter()
                    .map(|f| CleanupEntry {
                        token: f.token,
                        name: f.name,
                        created_time_secs: f.created_time_secs,
                        is_dir: f.is_dir,
                    })
                    .collect()
            })
            .map_err(|e| e.to_string())
    }
    fn delete(&self, file_token: &str) -> Result<(), String> {
        self.client.delete_file(file_token).map_err(|e| e.to_string())
    }
}

/* ===================== 工具 ===================== */

/// 月份目录名判定（D30）：`^\d{4}-\d{2}$`。手写等价判定，免引正则。
fn is_month_dir(name: &str) -> bool {
    let b = name.as_bytes();
    b.len() == 7
        && b[..4].iter().all(u8::is_ascii_digit)
        && b[4] == b'-'
        && b[5..].iter().all(u8::is_ascii_digit)
}

/// 日志脱敏（§5.5）：token 只记前 8 位
fn short_token(t: &str) -> String {
    t.chars().take(8).collect()
}

/// 锁内改写：解析磁盘值 → 调用方原地修改 → 原子写（全程持锁）
fn write_state(path: &Path, f: impl FnOnce(&mut CleanupState)) -> Result<(), String> {
    let (guard, disk) = statefile::with_exclusive(path)?;
    let mut st = CleanupState::parse(disk.as_ref());
    f(&mut st);
    write_under_guard(path, &st)?;
    drop(guard);
    Ok(())
}

/// 持 guard 原子写（guard 由调用方持有，写完即释放）
fn write_under_guard(path: &Path, st: &CleanupState) -> Result<(), String> {
    let v = serde_json::to_value(st).map_err(|e| format!("cleanup.json 序列化失败: {e}"))?;
    statefile::atomic_write(path, &v)
}

/* ===================== 单测（stub store + 固定 now，方案 §7 M1-4） ===================== */

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::{HashMap, HashSet};
    use std::path::PathBuf;
    use std::sync::Mutex;

    const NOW: i64 = 1_789_900_000;

    /* ===== stub store（Mutex 内部可变性，Sync，可跨线程共享） ===== */

    #[derive(Default)]
    struct StubStore {
        root: String,
        /// folder_token → 条目
        dirs: Mutex<HashMap<String, Vec<CleanupEntry>>>,
        deleted: Mutex<Vec<String>>,
        fail_delete: Mutex<HashSet<String>>,
        /// root_folder 调用次数（7 天门控并发只认领一轮的观测量）
        root_calls: Mutex<usize>,
        /// list_dir 调用的目录 token 序列（全部月份目录都被列举的观测量）
        list_calls: Mutex<Vec<String>>,
    }

    impl StubStore {
        fn new(root: &str) -> Self {
            Self { root: root.into(), ..Default::default() }
        }
        fn put(&self, folder: &str, entries: Vec<CleanupEntry>) {
            self.dirs.lock().unwrap().insert(folder.into(), entries);
        }
        fn fail_deletes(&self, tokens: &[&str]) {
            let mut f = self.fail_delete.lock().unwrap();
            for t in tokens {
                f.insert((*t).into());
            }
        }
    }

    impl CleanupStore for StubStore {
        fn root_folder(&self) -> Result<String, String> {
            *self.root_calls.lock().unwrap() += 1;
            Ok(self.root.clone())
        }
        fn list_dir(&self, folder_token: &str) -> Result<Vec<CleanupEntry>, String> {
            self.list_calls.lock().unwrap().push(folder_token.to_string());
            Ok(self.dirs.lock().unwrap().get(folder_token).cloned().unwrap_or_default())
        }
        fn delete(&self, token: &str) -> Result<(), String> {
            self.deleted.lock().unwrap().push(token.to_string());
            if self.fail_delete.lock().unwrap().contains(token) {
                Err(format!("注入删除失败 {token}"))
            } else {
                Ok(())
            }
        }
    }

    fn entry(token: &str, name: &str, created: i64, is_dir: bool) -> CleanupEntry {
        CleanupEntry { token: token.into(), name: name.into(), created_time_secs: created, is_dir }
    }

    /// 组一棵标准目录树：root → 青鸟传输 → 3 个月份目录（07/08/09），
    /// 外加同名文件、非月份目录、杂散根文件。返回 (store, month_tokens)。
    fn tree(now: i64) -> (StubStore, Vec<String>) {
        let st = StubStore::new("root");
        st.put("root", vec![
            entry("qtr", "青鸟传输", now - 90 * 86_400, true),
            entry("strayfile", "readme.txt", now - 90 * 86_400, false),
        ]);
        let months = ["2026-07", "2026-08", "2026-09"];
        let tokens: Vec<String> = months.iter().map(|m| format!("m-{m}")).collect();
        st.put("qtr", vec![
            entry("m-2026-07", "2026-07", now - 90 * 86_400, true),
            entry("m-2026-08", "2026-08", now - 60 * 86_400, true),
            entry("m-2026-09", "2026-09", now - 20 * 86_400, true),
            // 同名**文件**不被当目录（D30 类型过滤）
            entry("fake-2026-07", "2026-07", now - 90 * 86_400, false),
            // 非月份名的目录不扫
            entry("d-notes", "notes", now - 90 * 86_400, true),
        ]);
        (st, tokens)
    }

    fn temp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("qn-cleanup-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn read_state(dir: &Path) -> CleanupState {
        serde_json::from_str(&std::fs::read_to_string(dir.join(STATE_FILE)).unwrap()).unwrap()
    }

    /* ===== schedule_deletes ===== */

    #[test]
    fn schedule_deletes_is_idempotent_on_same_token() {
        let dir = temp_dir("dedup");
        let tokens = vec!["tok-a".to_string(), "tok-b".to_string()];
        assert_eq!(schedule_deletes(&dir, &tokens, NOW + 2400), 2);
        // 同 token 再次登记 → 跳过
        assert_eq!(schedule_deletes(&dir, &["tok-a".to_string()], NOW + 9999), 0);
        let st = read_state(&dir);
        assert_eq!(st.scheduled.len(), 2, "同 token 不得重复登记");
        assert_eq!(st.scheduled[0].at, NOW + 2400, "已有条目的 at 不得被改写");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn corrupt_cleanup_json_degrades_to_empty() {
        let dir = temp_dir("corrupt");
        std::fs::write(dir.join(STATE_FILE), "{{{ not json").unwrap();
        assert_eq!(schedule_deletes(&dir, &["tok".to_string()], NOW + 1), 1, "损坏文件按空处理");
        // version 不匹配同样降级
        std::fs::write(dir.join(STATE_FILE), r#"{"version":99,"scheduled":[]}"#).unwrap();
        assert_eq!(schedule_deletes(&dir, &["tok2".to_string()], NOW + 1), 1);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /* ===== 定时清理 flush ===== */

    #[test]
    fn flush_before_due_keeps_entry_after_due_deletes() {
        let dir = temp_dir("flush-due");
        let store = StubStore::new("root");
        schedule_deletes(&dir, &["tok-a".to_string()], NOW + 2400);
        // 预置「刚扫过」，隔离 7 天扫描变量，只测 flush
        {
            let mut prev = None;
            claim_sweep(&dir.join(STATE_FILE), NOW, &mut prev).unwrap();
        }
        // 到期前：不删
        let r = run_due(&store, &dir, NOW);
        assert!(!r.swept);
        assert_eq!((r.scheduled_flushed, r.flush_failed), (0, 0));
        assert!(store.deleted.lock().unwrap().is_empty());
        // 到期后：删
        let store2 = StubStore::new("root");
        let r = run_due(&store2, &dir, NOW + 2401);
        assert_eq!(r.scheduled_flushed, 1);
        assert_eq!(*store2.deleted.lock().unwrap(), vec!["tok-a".to_string()]);
        // 条目已在认领时移除
        assert!(read_state(&dir).scheduled.is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn flush_delete_failure_logs_only_and_never_retries() {
        let dir = temp_dir("flush-fail");
        let store = StubStore::new("root");
        store.fail_deletes(&["tok-a"]);
        schedule_deletes(&dir, &["tok-a".to_string()], NOW - 1);
        let r = run_due(&store, &dir, NOW);
        assert_eq!((r.scheduled_flushed, r.flush_failed), (0, 1));
        // 已认领即移除：不保留、不重试（D29）
        assert!(read_state(&dir).scheduled.is_empty());
        // 再跑一轮：不再尝试删除
        let store2 = StubStore::new("root");
        let r2 = run_due(&store2, &dir, NOW + 10);
        assert_eq!(r2.flush_failed, 0);
        assert!(store2.deleted.lock().unwrap().is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// U23：APP 与 CLI 同时 run_due → 同一 token 只被删一次（先认领后执行）
    #[test]
    fn concurrent_run_due_claims_token_once() {
        let dir = temp_dir("concurrent-claim");
        schedule_deletes(&dir, &["tok-x".to_string()], NOW - 1);
        let store = Arc::new(StubStore::new("root"));
        // last_sweep_at 置为刚扫过，隔离扫描变量，只测 flush 认领
        {
            let store = store.clone();
            let dir2 = dir.clone();
            let h = std::thread::spawn(move || run_due(store.as_ref(), &dir2, NOW));
            h.join().unwrap();
        }
        let deleted_after_setup = store.deleted.lock().unwrap().len();
        let mut handles = Vec::new();
        for _ in 0..4 {
            let store = store.clone();
            let dir = dir.clone();
            handles.push(std::thread::spawn(move || run_due(store.as_ref(), &dir, NOW)));
        }
        let mut total_flushed = 0;
        for h in handles {
            total_flushed += h.join().unwrap().scheduled_flushed;
        }
        assert_eq!(total_flushed, 0, "认领过的条目不得被重复认领");
        assert_eq!(store.deleted.lock().unwrap().len(), deleted_after_setup);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 真·并发认领：同一到期条目 + 4 线程同时开跑 → 恰好一次删除
    #[test]
    fn concurrent_first_claim_deletes_exactly_once() {
        let dir = temp_dir("concurrent-first");
        schedule_deletes(&dir, &["tok-y".to_string()], NOW - 1);
        let store = Arc::new(StubStore::new("root"));
        let barrier = Arc::new(std::sync::Barrier::new(4));
        let mut handles = Vec::new();
        for _ in 0..4 {
            let store = store.clone();
            let dir = dir.clone();
            let barrier = barrier.clone();
            handles.push(std::thread::spawn(move || {
                barrier.wait();
                run_due(store.as_ref(), &dir, NOW)
            }));
        }
        let mut total_flushed = 0;
        for h in handles {
            total_flushed += h.join().unwrap().scheduled_flushed;
        }
        assert_eq!(total_flushed, 1, "四个并发进程同时开跑，同 token 只能被认领删除一次");
        assert_eq!(*store.deleted.lock().unwrap(), vec!["tok-y".to_string()]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /* ===== 每周扫描 ===== */

    #[test]
    fn sweep_boundary_48h_strictly_greater() {
        let dir = temp_dir("sweep-boundary");
        let (store, months) = tree(NOW);
        // 恰好 48h：不删（严格大于）；48h+1s：删
        store.put(&months[2], vec![
            entry("c-exact", "q2-aaaaaaaa", NOW - ORPHAN_AGE_SECS, false),
            entry("c-over", "q2-bbbbbbbb", NOW - ORPHAN_AGE_SECS - 1, false),
            // 缺失 created_time（0）不可判龄：跳过
            entry("c-notime", "q2-cccccccc", 0, false),
        ]);
        let r = run_due(&store, &dir, NOW);
        assert!(r.swept);
        assert_eq!(r.orphan_deleted, 1);
        assert_eq!(*store.deleted.lock().unwrap(), vec!["c-over".to_string()]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn sweep_gate_7d_blocks_until_due() {
        let dir = temp_dir("sweep-gate");
        let (store, _months) = tree(NOW);
        // 门控内（只差 1 秒到 7 天）：不扫、不发起任何列举
        {
            let mut prev = None;
            claim_sweep(&dir.join(STATE_FILE), NOW - SWEEP_INTERVAL_SECS + 1, &mut prev).unwrap();
        }
        let r = run_due(&store, &dir, NOW);
        assert!(!r.swept);
        assert!(store.list_calls.lock().unwrap().is_empty(), "门控内不得发起任何 list 调用");
        // 门控到期：恰好 7 天（>=）即扫
        let r = run_due(&store, &dir, NOW + SWEEP_INTERVAL_SECS);
        assert!(r.swept);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn sweep_gate_concurrent_claims_only_one_round() {
        let dir = temp_dir("sweep-gate-x");
        let store = Arc::new(tree(NOW).0);
        let barrier = Arc::new(std::sync::Barrier::new(2));
        let mut handles = Vec::new();
        for _ in 0..2 {
            let store = store.clone();
            let dir = dir.clone();
            let barrier = barrier.clone();
            handles.push(std::thread::spawn(move || {
                barrier.wait();
                run_due(store.as_ref(), &dir, NOW)
            }));
        }
        let swept = h_sum(handles);
        assert_eq!(swept, 1, "7 天门控并发只允许认领一轮扫描");
        assert_eq!(*store.root_calls.lock().unwrap(), 1, "root_folder 只被认领者调用一次");
        let _ = std::fs::remove_dir_all(&dir);
    }

    fn h_sum(handles: Vec<std::thread::JoinHandle<CleanupReport>>) -> u32 {
        handles.into_iter().map(|h| h.join().unwrap().swept as u32).sum()
    }

    #[test]
    fn sweep_lists_every_month_dir_and_skips_same_name_file() {
        let dir = temp_dir("sweep-months");
        let (store, months) = tree(NOW);
        run_due(&store, &dir, NOW);
        let calls = store.list_calls.lock().unwrap();
        for m in &months {
            assert!(calls.contains(m), "月份目录 {m} 必须被列举（全部月份，D30）");
        }
        assert!(!calls.contains(&"fake-2026-07".to_string()), "同名文件不得当目录列举");
        assert!(!calls.contains(&"d-notes".to_string()), "非 YYYY-MM 目录不得列举");
        // list_dir 调用数 = 2 + N（列根 1 + 列「青鸟传输」1 + N 个月份目录）；
        // 方案 §6 #4 口径的「3 + N」把 root meta 那次也计入（同一 list 配额 kind），
        // root meta 在 trait 层由 root_folder 承担，不计入 list_calls。
        assert_eq!(calls.len(), 2 + months.len(), "一轮扫描的 list_dir 调用数应为 2 + N");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn sweep_failure_on_one_delete_does_not_stop_others() {
        let dir = temp_dir("sweep-fail");
        let (store, months) = tree(NOW);
        store.put(&months[2], vec![
            entry("c-1", "q2-dddddddd", NOW - ORPHAN_AGE_SECS - 10, false),
            entry("c-2", "q2-eeeeeeee", NOW - ORPHAN_AGE_SECS - 10, false),
            entry("c-3", "q2-ffffffff", NOW - ORPHAN_AGE_SECS - 10, false),
        ]);
        store.fail_deletes(&["c-2"]);
        let r = run_due(&store, &dir, NOW);
        assert!(r.swept);
        assert_eq!(r.orphan_deleted, 2, "单条失败不中断整体");
        assert_eq!(r.orphan_failed, 1);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn sweep_without_transfer_dir_reports_note_only() {
        let dir = temp_dir("sweep-empty");
        let store = StubStore::new("root");
        store.put("root", vec![]);
        let r = run_due(&store, &dir, NOW);
        assert!(r.swept);
        assert!(r.note.is_some(), "找不到「青鸟传输」应记入 note");
        assert_eq!(r.orphan_deleted + r.orphan_failed, 0);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /* ===== run_due_feishu 早退 ===== */

    #[test]
    fn run_due_feishu_noop_without_app_id() {
        let dir = temp_dir("noop");
        let quota = Arc::new(Quota::open(&dir).unwrap());
        let r = run_due_feishu(&dir, "", "secret", quota, NOW);
        assert!(!r.swept);
        assert_eq!(r.scheduled_flushed + r.flush_failed + r.orphan_deleted + r.orphan_failed, 0);
        assert!(r.note.is_some());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /* ===== 纯函数 ===== */

    #[test]
    fn month_dir_name_filter() {
        assert!(is_month_dir("2026-09"));
        assert!(is_month_dir("1970-01"));
        assert!(!is_month_dir("2026-9"));
        assert!(!is_month_dir("20269-1"));
        assert!(!is_month_dir("2026_09"));
        assert!(!is_month_dir("abcd-ef"));
        assert!(!is_month_dir("2026-091"));
        assert!(!is_month_dir(""));
        assert!(!is_month_dir("2026-0a"));
    }
}
