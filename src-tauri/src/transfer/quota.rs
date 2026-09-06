//! 月度 API 额度计数（设计文档 §6.5，P1-1）。
//!
//! 计数模型：本地持久化 `quota.json`
//! `{"month":"2026-09","used":n,"by_type":{"upload":..,"download":..,"delete":..,"list":..}}`
//! - 跨自然月自动重置
//! - 每次 Drive API 调用**发出即计数（含失败与重试）**
//! - tenant_access_token 换取不计数；webhook 发消息不计数
//! - P4 二批：月累计 ≥ 8,000 时拒绝新上传（`will_block` 预留）

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Mutex;

#[derive(Debug, Default, Serialize, Deserialize)]
struct QuotaData {
    /// 自然月，如 "2026-09"
    month: String,
    used: u64,
    by_type: BTreeMap<String, u64>,
}

pub struct Quota {
    path: PathBuf,
    inner: Mutex<QuotaData>,
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
        Ok(Self { path, inner: Mutex::new(data) })
    }

    fn current_month() -> String {
        let now = time::OffsetDateTime::now_utc();
        format!("{:04}-{:02}", now.year(), u8::from(now.month()))
    }

    /// 记一次调用（发出即计数），返回当前月累计
    pub fn bump(&self, kind: &str) -> u64 {
        let mut d = self.inner.lock().unwrap_or_else(|p| p.into_inner());
        let month = Self::current_month();
        if d.month != month {
            // 跨自然月重置（§6.5）
            d.month = month;
            d.used = 0;
            d.by_type.clear();
        }
        d.used += 1;
        *d.by_type.entry(kind.to_string()).or_insert(0) += 1;
        let used = d.used;
        self.persist(&d);
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
            self.persist(&d);
        }
        d.used
    }

    /// P4 预留：月累计 ≥ 8,000 时应阻断新上传（§6.5）
    pub fn will_block(&self) -> bool {
        self.used() >= 8_000
    }

    fn persist(&self, d: &QuotaData) {
        if let Ok(json) = serde_json::to_string_pretty(d) {
            let tmp = self.path.with_extension("json.tmp");
            if std::fs::write(&tmp, json).is_ok() {
                let _ = std::fs::rename(&tmp, &self.path);
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
}
