//! 本地服务契约与阶段 A 替身
//!
//! 对应设计文档 §7.3.1（传输活动聚合快照）、§9.1/§9.2（服务状态与菜单映射）、
//! §9.6（适配接口契约）。
//!
//! 阶段 A 只依赖本模块的**接口**，不依赖文件传输实现；阶段 B 用真实实现替换 `Fake*`。
//! 只读与副作用必须分属不同接口，否则退出停机与端口变更无法用 fake 验收。

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// 本地服务默认端口（自 M0c 起真源在 core：链接端口口径 `configured_port` 的缺省值）。
///
/// 此处仅转发既有引用。除作为 `configured_port` 的缺省值外，本 crate 的 debug 解析兜底
/// 与测试用 fake 端口也取自它。菜单文案禁止硬编码该字面量（§9.2）。
pub use qingniao_core::transfer::engine::DEFAULT_LOCAL_PORT;

/// 服务启动失败原因（§9.2）
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ServiceFailure {
    /// 端口被占用——此时不允许自动漂移到随机端口（D4）
    PortInUse,
    /// 其他失败
    Other,
}

/// 本地服务状态（§9.2）。带数据的快照，菜单文案只取状态携带的端口。
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum LocalServiceStatus {
    Starting,
    /// `bound_port` 仅在服务成功启动时存在
    Running { bound_port: u16 },
    Failed {
        kind: ServiceFailure,
        requested_port: u16,
    },
    Stopping,
    #[default]
    Stopped,
}

impl LocalServiceStatus {
    /// 菜单展示文案。端口一律来自状态字段，不得硬编码 9876。
    pub fn menu_label(&self) -> String {
        match self {
            Self::Starting => "本地服务：启动中…".to_string(),
            Self::Running { bound_port } => {
                format!("本地服务：运行中 · 127.0.0.1:{bound_port}")
            }
            Self::Failed {
                kind: ServiceFailure::PortInUse,
                requested_port,
            } => format!("本地服务：启动失败（端口 {requested_port} 被占用）"),
            Self::Failed {
                kind: ServiceFailure::Other,
                ..
            } => "本地服务：启动失败".to_string(),
            Self::Stopping => "本地服务：正在停止…".to_string(),
            Self::Stopped => "本地服务：已停止".to_string(),
        }
    }
}

/// 传输活动**聚合**快照（§7.3.1）。
///
/// 刻意不用互斥枚举表达：桌面文件服务可能**同时**存在上传、下载、校验与提交任务。
///
/// **关键约定**：HTTP listener 常驻运行本身不算 active task——服务从启动到退出全程常驻，
/// 若把「服务活跃」当作忙碌，会导致每次退出都弹确认。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct TransferSnapshot {
    /// listener 是否接受新请求
    pub accepting: bool,
    /// 可取消任务数（上传 / 下载 / 解密校验）
    pub cancellable_count: u32,
    /// 处于原子提交临界区（`Renaming`）的任务数
    pub committing_count: u32,
    /// 云端分片删除待重试数（已持久化则不阻止退出）
    pub cleanup_pending_count: u32,
    /// 本次 drain 是否已落检查点——决定超时文案（§7.2.3）
    pub checkpointed: bool,
}

impl TransferSnapshot {
    /// **唯一**退出判定（§7.3.1）：
    /// 无可取消任务且无原子提交任务 → 直接退出，不弹确认。
    ///
    /// `cleanup_pending_count` **单独存在时不触发确认**：其状态已持久化，可下次启动恢复。
    pub fn requires_confirm(&self) -> bool {
        self.cancellable_count > 0 || self.committing_count > 0
    }

    /// 是否处于不可中断的原子提交临界区（`Renaming`）。该区不套用 15 s 强退上限（§7.2.3）。
    pub fn in_atomic_critical_section(&self) -> bool {
        self.committing_count > 0
    }
}

/// 权威状态持有者：真实服务与阶段 A 替身共用同一状态源（§5.2 唯一推送点的存储）
pub struct StatusCell(Mutex<LocalServiceStatus>);

impl StatusCell {
    pub fn new(status: LocalServiceStatus) -> Self {
        Self(Mutex::new(status))
    }
    pub fn set(&self, status: LocalServiceStatus) {
        *self.0.lock().unwrap() = status;
    }
}

impl ServiceStatusProvider for StatusCell {
    fn snapshot(&self) -> LocalServiceStatus {
        self.0.lock().unwrap().clone()
    }
}

/// 只读：服务状态（§9.6）
pub trait ServiceStatusProvider: Send + Sync + 'static {
    fn snapshot(&self) -> LocalServiceStatus;
}

/// 副作用：listener 生命周期（§9.6）
pub trait LocalServiceController: Send + Sync + 'static {
    /// **barrier 语义**：返回时，此前已接受的请求**已注册进快照**；此后到达的请求一律拒绝。
    fn stop_accepting(&self);
    /// 用户取消退出时恢复准入；调用方须处理恢复失败（进入可见错误状态）
    fn resume_accepting(&self);
    fn stop(&self);
    fn restart(&self, port: u16);
}

/// 只读：传输活动（§9.6）
pub trait TransferActivityProvider: Send + Sync + 'static {
    fn snapshot(&self) -> TransferSnapshot;
}

/// 副作用：drain 与安全点（§9.6）
pub trait TransferController: Send + Sync + 'static {
    fn checkpoint(&self);
    fn cancel_all(&self);
    /// 等待到达安全点；返回 `true` 表示在 `timeout` 内完成
    fn join_to_safe_point(&self, timeout: Duration) -> bool;
}

/// 副作用：菜单到前端路由（§9.6）
pub trait WindowRouteSink: Send + Sync + 'static {
    fn navigate(&self, route: &str);
}

// ---------------------------------------------------------------------------
// 阶段 A 的 fake 实现（供 §15.1 的 unit/fake 用例使用；阶段 B 由真实实现替换）
// ---------------------------------------------------------------------------

/// 可在测试中驱动状态与记录调用的服务替身
#[derive(Default)]
pub struct FakeLocalService {
    status: Mutex<LocalServiceStatus>,
    accepting: Mutex<bool>,
    stopped: AtomicBool,
    restart_calls: Mutex<Vec<u16>>,
    stop_accepting_calls: Mutex<u32>,
}

impl FakeLocalService {
    pub fn new(status: LocalServiceStatus) -> Self {
        Self {
            status: Mutex::new(status),
            accepting: Mutex::new(true),
            stopped: AtomicBool::new(false),
            restart_calls: Mutex::new(Vec::new()),
            stop_accepting_calls: Mutex::new(0),
        }
    }

    pub fn set_status(&self, status: LocalServiceStatus) {
        *self.status.lock().unwrap() = status;
    }

    pub fn accepting(&self) -> bool {
        *self.accepting.lock().unwrap()
    }

    pub fn is_stopped(&self) -> bool {
        self.stopped.load(Ordering::SeqCst)
    }

    pub fn restart_calls(&self) -> Vec<u16> {
        self.restart_calls.lock().unwrap().clone()
    }

    pub fn stop_accepting_calls(&self) -> u32 {
        *self.stop_accepting_calls.lock().unwrap()
    }
}

impl ServiceStatusProvider for FakeLocalService {
    fn snapshot(&self) -> LocalServiceStatus {
        self.status.lock().unwrap().clone()
    }
}

impl LocalServiceController for FakeLocalService {
    fn stop_accepting(&self) {
        *self.stop_accepting_calls.lock().unwrap() += 1;
        *self.accepting.lock().unwrap() = false;
    }
    fn resume_accepting(&self) {
        *self.accepting.lock().unwrap() = true;
    }
    fn stop(&self) {
        self.stopped.store(true, Ordering::SeqCst);
        self.set_status(LocalServiceStatus::Stopped);
    }
    fn restart(&self, port: u16) {
        self.restart_calls.lock().unwrap().push(port);
        self.set_status(LocalServiceStatus::Running { bound_port: port });
    }
}

/// 可编程的传输活动替身
pub struct FakeTransfer {
    snapshot: Mutex<TransferSnapshot>,
    pub checkpoint_count: Mutex<u32>,
    pub cancel_count: Mutex<u32>,
    /// `join_to_safe_point` 的返回值——用于模拟「15 s 超时未达安全点」
    pub join_succeeds: AtomicBool,
}

impl Default for FakeTransfer {
    fn default() -> Self {
        Self {
            snapshot: Mutex::new(TransferSnapshot::default()),
            checkpoint_count: Mutex::new(0),
            cancel_count: Mutex::new(0),
            join_succeeds: AtomicBool::new(true),
        }
    }
}

impl FakeTransfer {
    pub fn new(snapshot: TransferSnapshot) -> Self {
        Self {
            snapshot: Mutex::new(snapshot),
            ..Default::default()
        }
    }

    pub fn set_snapshot(&self, snapshot: TransferSnapshot) {
        *self.snapshot.lock().unwrap() = snapshot;
    }
}

impl TransferActivityProvider for FakeTransfer {
    fn snapshot(&self) -> TransferSnapshot {
        *self.snapshot.lock().unwrap()
    }
}

impl TransferController for FakeTransfer {
    fn checkpoint(&self) {
        *self.checkpoint_count.lock().unwrap() += 1;
        self.snapshot.lock().unwrap().checkpointed = true;
    }
    fn cancel_all(&self) {
        *self.cancel_count.lock().unwrap() += 1;
        self.snapshot.lock().unwrap().cancellable_count = 0;
    }
    fn join_to_safe_point(&self, _timeout: Duration) -> bool {
        self.join_succeeds.load(Ordering::SeqCst)
    }
}

/// 记录路由跳转的替身
#[derive(Default)]
pub struct FakeRouteSink {
    pub routes: Mutex<Vec<String>>,
}

impl WindowRouteSink for FakeRouteSink {
    fn navigate(&self, route: &str) {
        self.routes.lock().unwrap().push(route.to_string());
    }
}

/// 服务与传输的集合，放进 `AppState` 供菜单与退出协议使用。
/// 阶段 B（文件传输）以真实实现替换 fake：状态源为 [`StatusCell`]，
/// 控制器为 `local_server::RealLocalService`，传输活动为引擎适配器。
pub struct PhaseAService {
    /// 权威状态源（menu_label 文案的依据）
    pub status: Arc<StatusCell>,
    /// 副作用：listener 生命周期（stop/restart/stop_accepting）
    pub service: Arc<dyn LocalServiceController>,
    /// 只读：传输活动快照
    pub transfer: Arc<dyn TransferActivityProvider>,
    /// 副作用：drain 与安全点
    pub transfer_ctrl: Arc<dyn TransferController>,
    pub route_sink: Arc<FakeRouteSink>,
}

impl PhaseAService {
    /// 阶段 A 替身集合（单测/降级用）：服务处于「运行中（默认端口）」，无进行中任务
    pub fn new() -> Self {
        let fake = Arc::new(FakeLocalService::new(LocalServiceStatus::Running {
            bound_port: DEFAULT_LOCAL_PORT,
        }));
        let fake_transfer = Arc::new(FakeTransfer::default());
        Self {
            status: Arc::new(StatusCell::new(LocalServiceStatus::Running {
                bound_port: DEFAULT_LOCAL_PORT,
            })),
            service: fake,
            transfer: fake_transfer.clone(),
            transfer_ctrl: fake_transfer,
            route_sink: Arc::new(FakeRouteSink::default()),
        }
    }

    /// 真实装配（setup 时调用）
    pub fn with_parts(
        status: Arc<StatusCell>,
        service: Arc<dyn LocalServiceController>,
        transfer: Arc<dyn TransferActivityProvider>,
        transfer_ctrl: Arc<dyn TransferController>,
    ) -> Self {
        Self {
            status,
            service,
            transfer,
            transfer_ctrl,
            route_sink: Arc::new(FakeRouteSink::default()),
        }
    }
}

impl Default for PhaseAService {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A13：空闲退出——listener 在跑但无任务、仅有已持久化的 cleanup 待重试 → 不弹确认
    #[test]
    fn idle_with_pending_cleanup_does_not_require_confirm() {
        let snap = TransferSnapshot {
            accepting: true,
            cancellable_count: 0,
            committing_count: 0,
            cleanup_pending_count: 3,
            checkpointed: false,
        };
        assert!(!snap.requires_confirm());
    }

    /// A12：有可取消任务或处于原子提交 → 必须确认
    #[test]
    fn busy_states_require_confirm() {
        let downloading = TransferSnapshot {
            cancellable_count: 1,
            ..Default::default()
        };
        assert!(downloading.requires_confirm());

        let committing = TransferSnapshot {
            committing_count: 1,
            ..Default::default()
        };
        assert!(committing.requires_confirm());
        assert!(committing.in_atomic_critical_section());
        assert!(!downloading.in_atomic_critical_section());
    }

    /// A8：菜单文案只取状态携带的端口，非默认端口也正确
    #[test]
    fn menu_label_uses_port_from_state() {
        assert_eq!(
            LocalServiceStatus::Running { bound_port: 9876 }.menu_label(),
            "本地服务：运行中 · 127.0.0.1:9876"
        );
        assert_eq!(
            LocalServiceStatus::Running { bound_port: 12345 }.menu_label(),
            "本地服务：运行中 · 127.0.0.1:12345"
        );
        assert_eq!(
            LocalServiceStatus::Failed {
                kind: ServiceFailure::PortInUse,
                requested_port: 12345,
            }
            .menu_label(),
            "本地服务：启动失败（端口 12345 被占用）"
        );
    }

    /// A10：restart 入参被正确传递
    #[test]
    fn restart_records_requested_port() {
        let svc = FakeLocalService::new(LocalServiceStatus::Stopped);
        svc.restart(12345);
        assert_eq!(svc.restart_calls(), vec![12345]);
        assert_eq!(
            svc.snapshot(),
            LocalServiceStatus::Running { bound_port: 12345 }
        );
    }

    /// A19 的一部分：门闩的 barrier 语义——调用后准入关闭
    #[test]
    fn stop_accepting_closes_admission() {
        let svc = FakeLocalService::new(LocalServiceStatus::Stopped);
        assert!(svc.accepting());
        svc.stop_accepting();
        assert!(!svc.accepting());
        svc.resume_accepting();
        assert!(svc.accepting());
    }
}
