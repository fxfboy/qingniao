//! 青鸟共享核心：APP（Tauri）与 CLI 共用的实现（**零 tauri 依赖**）。
//!
//! **行为基线** = 仓库根 `tests/golden/golden.json`，**仅覆盖 `message` 模块**
//! （M0 从 `src/message.js` 真实实现捕获，见 Agent-CLI 方案 v3 §四 D12）。
//!
//! 其余模块各以单测与固定值 fixture 为基线：
//! - `config` / `send` / `skills`：单测
//! - `transfer`：`tests/golden/transfer-interop.json`（跨版本互操作 + 加密字节确定性，
//!   见 CLI 文件传输方案 §10 M0a 出口 1/2）与 `transfer` 模块内单测
//!
//! **任何行为变更必须先显式重新捕获对应基线并说明原因。**
//!
//! > 这条声明在 v1.3 之前写成「任何行为变更必须先重新捕获 golden」，与实际不符——
//! > `config`/`send`/`skills` 从来不在 golden 覆盖内（C1 评审 P0-1 指出）。

pub mod config;
pub mod keyring_store;
pub mod message;
pub mod send;
pub mod skills;
pub mod transfer;
