//! 传输模块的 **APP 侧适配层**（CLI 文件传输方案 v0.4 §6）。
//!
//! 传输实现已迁入 `qingniao_core::transfer`（**零 tauri 依赖**）。本模块只做两件事：
//!
//! 1. **转发** core 的子模块，使 `crate::transfer::crypto::…` 这类既有路径继续可用
//!    （`local_server.rs` 等处的引用因此不必全量改写）；
//! 2. 实现 [`Host`]——把 AppHandle 能提供的五件事（配置目录 / 下载目录 / 本地服务端口 /
//!    进度事件 / 完成事件）注入引擎。**这是 tauri 耦合的唯一落点。**
//!
//! `local_server.rs` **不在此列**：它本身零 tauri 依赖，留在 `src-tauri` 只是因为
//! 它服务于浏览器确认页（APP 独有），且引擎去 tauri 化后它只需 `use` core 的 `Engine`。

pub use qingniao_core::transfer::{crypto, engine, feishu, quota};

use engine::Host;
use std::path::PathBuf;
use std::sync::Arc;
use tauri::{Emitter, Manager};

/// APP 侧宿主实现。
///
/// 每个方法都与 M0a 之前引擎内联调用 `AppHandle` 的那一行**逐点等价**——
/// 这是「纯搬运、行为零变更」的落点，任何差异都会让 M0a 出口 1/2 的基线失效。
pub struct TauriHost {
    app: tauri::AppHandle,
}

impl TauriHost {
    pub fn new(app: tauri::AppHandle) -> Self {
        Self { app }
    }

    /// 引擎构造需要 `Arc<dyn Host>`
    pub fn into_arc(app: tauri::AppHandle) -> Arc<dyn Host> {
        Arc::new(Self::new(app))
    }
}

impl Host for TauriHost {
    fn config_dir(&self) -> Result<PathBuf, String> {
        self.app
            .path()
            .app_config_dir()
            .map_err(|e| format!("无法定位配置目录: {e}"))
    }

    /// 等价于迁移前的 `engine::download_dir_of(app)`（逐行照搬）
    fn resolve_download_dir(&self) -> Result<String, String> {
        let cfg_path = self.config_dir()?.join("qingniao.json");
        let configured: Option<String> = std::fs::read_to_string(&cfg_path)
            .ok()
            .and_then(|s| serde_json::from_str::<serde_json::Value>(&s).ok())
            .and_then(|v| {
                v.pointer("/transfer/download_dir")
                    .and_then(|d| d.as_str())
                    .map(String::from)
            });
        let dir = match configured {
            Some(d) if !d.is_empty() => PathBuf::from(d),
            _ => self
                .app
                .path()
                .download_dir()
                .map_err(|e| format!("无法定位系统下载目录: {e}"))?,
        };
        std::fs::create_dir_all(&dir).map_err(|e| format!("创建下载目录失败: {e}"))?;
        Ok(dir.to_string_lossy().to_string())
    }

    fn service_bound_port(&self) -> Option<u16> {
        crate::service_bound_port(&self.app)
    }

    fn progress(&self, payload: serde_json::Value) {
        let _ = self.app.emit("transfer://progress", payload);
    }

    fn downloaded(&self, payload: serde_json::Value) {
        let _ = self.app.emit("transfer://downloaded", payload);
    }
}
