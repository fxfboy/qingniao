//! 跨网文件传输（docs/文件传输功能设计文档.md v1.3）
//!
//! - [`crypto`]：E2E 加密协议（payload 信封 / 分片加解密 / kid / 文件名净化）
//! - [`feishu`]：飞书 Drive API 客户端
//! - [`engine`]：传输引擎（任务生命周期 / 下载会话 / 已消费缓存）
//! - [`quota`]：月度 API 额度计数
//! - [`cleanup`]：云端清理（发送端定时清理 + 每周 48 h 孤儿扫描，清理方案 v0.3）
//!
//! 本模块**零 tauri 依赖**：与宿主环境的全部交互收敛到 [`engine::Host`]，
//! APP 传 Tauri 实现（`src-tauri/src/transfer/mod.rs`），CLI 与测试传固定值实现。

pub mod cleanup;
pub mod crypto;
pub mod engine;
pub mod feishu;
pub mod quota;
pub mod statefile;
