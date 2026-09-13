//! 月度 API 额度计数（设计文档 §6.5，P1-1）。
//!
//! 计数模型：本地持久化 `quota.json`
//! `{"month":"2026-09","used":n,"by_type":{"upload":..,"download":..,"delete":..,"list":..}}`
//! - 跨自然月自动重置
//! - 每次 Drive API 调用**发出即计数（含失败与重试）**
//! - tenant_access_token 换取不计数；webhook 发消息不计数
//! - P4 二批：月累计 ≥ 8,000 时拒绝新上传（`will_block` 预留）
//!
//! 跨进程一致性（D24，M0b）：内存只记「自上次落盘以来的增量」，
//! 落盘时**锁内重读**磁盘最新值并把增量合并进去——两个进程交替 bump 不丢计数（U19 ①）。

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Mutex;

#[derive(Debug, Default, Serialize, Deserialize, Clone)]
struct QuotaData {
    /// 自然月，如 "2026-09"
    month: String,
    used: u64,
    by_type: BTreeMap<String, u64>,
}

#[derive(Debug, Default)]
struct Pending {
    /// 自上次落盘以来的增量（跨进程合并用）
    used: u64,
    by_type: BTreeMap<String, u64>,
}

pub struct Quota {
    path: PathBuf,
    inner: Mutex<QuotaData>,
    pending: Mutex<Pending>,
}

impl Quota {
    /// `dir` = <app_config_dir>/transfer
    pub fn open(dir: &std::path::Path) -> Result<Self, String> {
        std::fs::create_dir_all(dir).map_err(|e| format!("创建 transfer 目录失败: {e}"))?;
        let path = dir.join("quota.json");
        let data: QuotaData = std::fs::read_to_string(&path)
            .ok()
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default();
        Ok(Self {
            path,
            inner: Mutex::new(data),
            pending: Mutex::new(Pending::default()),
        })
    }

    fn current_month() -> String {
        let now = time::OffsetDateTime::now_utc();
        format!("{:04}-{:02}", now.year(), u8::from(now.month()))
    }

    /// 记一次调用（发出即计数），返回当前月累计
    /// 注意：inner/pending 两把锁都**不跨 persist 调用**持有——persist 内有文件锁内 IO，
    /// 与 inner 嵌套即同线程自死锁（M0b 实测教训）。
    pub fn bump(&self, kind: &str) -> u64 {
        let used = {
            let mut d = self.inner.lock().unwrap_or_else(|p| p.into_inner());
            let month = Self::current_month();
            if d.month != month {
                // 跨自然月重置（§6.5）；旧月增量随之作废
                d.month = month.clone();
                d.used = 0;
                d.by_type.clear();
                let mut p = self.pending.lock().unwrap_or_else(|p| p.into_inner());
                p.used = 0;
                p.by_type.clear();
            }
            d.used += 1;
            *d.by_type.entry(kind.to_string()).or_insert(0) += 1;
            d.used
        };
        {
            let mut p = self.pending.lock().unwrap_or_else(|p| p.into_inner());
            p.used += 1;
            *p.by_type.entry(kind.to_string()).or_insert(0) += 1;
        }
        self.persist();
        used
    }

    /// 当前月累计
    pub fn used(&self) -> u64 {
        let mut d = self.inner.lock().unwrap_or_else(|p| p.into_inner());
        let month = Self::current_month();
        if d.month != month {
            d.month = month;
            d.used = 0;
            d.by_type.clear();
            let mut p = self.pending.lock().unwrap_or_else(|p| p.into_inner());
            p.used = 0;
            p.by_type.clear();
        }
        d.used
    }

    /// P4 预留：月累计 ≥ 8,000 时应阻断新上传（§6.5）
    pub fn will_block(&self) -> bool {
        self.used() >= 8_000
    }

    /// 落盘：**锁内重读**磁盘（另一进程可能已写入）→ 合并本进程增量 → 原子写（D24/U19）
    fn persist(&self) {
        let (used_delta, by_delta) = {
            let mut p = self.pending.lock().unwrap_or_else(|p| p.into_inner());
            if p.used == 0 {
                return;
            }
            let s = (p.used, p.by_type.clone());
            p.used = 0;
            p.by_type.clear();
            s
        };
        let month = Self::current_month();
        let r = super::statefile::update::<QuotaData, _>(&self.path, |disk| {
            let mut merged = match disk {
                Some(v) => serde_json::from_value::<QuotaData>(v.clone()).unwrap_or_default(),
                None => QuotaData::default(),
            };
            if merged.month != month {
                merged = QuotaData { month: month.clone(), used: 0, by_type: BTreeMap::new() };
            }
            merged.used += used_delta;
            for (k, n) in &by_delta {
                *merged.by_type.entry(k.clone()).or_insert(0) += n;
            }
            Ok(merged)
        });
        if r.is_ok() {
            // 内存视图以合并后的磁盘值为准（另一进程可能同时写入了更多）
            let fresh: QuotaData = std::fs::read_to_string(&self.path)
                .ok()
                .and_then(|s| serde_json::from_str(&s).ok())
                .unwrap_or_default();
            let mut d = self.inner.lock().unwrap_or_else(|p| p.into_inner());
            if fresh.month == month {
                *d = fresh;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bump_accumulates_and_persists() {
        let dir = std::env::temp_dir().join(format!("qn-quota-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let q = Quota::open(&dir).unwrap();
        assert_eq!(q.bump("upload"), 1);
        assert_eq!(q.bump("upload"), 2);
        assert_eq!(q.bump("download"), 3);
        assert_eq!(q.used(), 3);
        // 重新打开读回
        let q2 = Quota::open(&dir).unwrap();
        assert_eq!(q2.used(), 3);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// D24/U19 ①：两个进程各自的 Quota 实例交替 bump，磁盘计数 = 总调用数
    #[test]
    fn quota_merge_across_instances() {
        let dir = std::env::temp_dir().join(format!("qn-quota-xp-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let a = Quota::open(&dir).unwrap();
        let b = Quota::open(&dir).unwrap();
        a.bump("upload"); // A: 1
        b.bump("upload"); // B: 磁盘 1 + 1 = 2
        b.bump("download"); // B: 3
        a.bump("upload"); // A: 磁盘 3 + 1 = 4
        let c = Quota::open(&dir).unwrap();
        assert_eq!(c.used(), 4, "跨实例合并后计数必须等于总调用数");
        let d = serde_json::from_str::<QuotaData>(&std::fs::read_to_string(dir.join("quota.json")).unwrap()).unwrap();
        assert_eq!(d.by_type.get("upload"), Some(&3));
        assert_eq!(d.by_type.get("download"), Some(&1));
        // 临时文件不得残留（pid 后缀原子写）
        assert!(!dir.join("quota.json.tmp").exists());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
