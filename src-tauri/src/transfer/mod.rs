//! 文件传输模块（docs/文件传输功能设计文档.md v1.1）
//!
//! - [`crypto`]：E2E 加密协议（payload 信封 / 分片加解密 / kid / 文件名净化）
//! - [`feishu`]：飞书 Drive API 客户端（P2）
//! - [`engine`]：传输引擎（P2/P3）

pub mod crypto;
pub mod engine;
pub mod feishu;
pub mod quota;

/// engine 内部回调 lib.rs 的辅助函数（避免循环 import 的转发层）
pub(crate) fn service_bound_port(app: &tauri::AppHandle) -> Option<u16> {
    crate::service_bound_port(app)
}

pub(crate) fn engine_quota_of(
    app: &tauri::AppHandle,
) -> Result<std::sync::Arc<quota::Quota>, String> {
    crate::engine_quota_of(app)
}
