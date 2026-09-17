//! 青鸟共享核心：APP（Tauri）与 CLI 共用的消息组装 / 识别 / 签名逻辑。
//!
//! 行为基线 = 仓库根 `tests/golden/golden.json`（M0 从 `src/message.js` 真实实现捕获，
//! 见 Agent-CLI 方案 v3 §四 D12）。任何行为变更必须先显式重新捕获基线并说明原因。

pub mod config;
pub mod message;
pub mod send;
pub mod skills;
