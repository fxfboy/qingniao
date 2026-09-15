//! 统一退出协议与退出状态机
//!
//! 对应设计文档 §7.2（§7.2.1 状态机、§7.2.2 停机顺序含退入门闩、§7.2.3 超时语义、§7.2.4 幂等性）。
//!
//! 关键不变量：
//! - 仅 `Exiting` 放行 `RunEvent::ExitRequested`；`Running`/`Confirming`/`Draining` 一律 `prevent_exit()`。
//!   （`AppHandle::exit()` 自身会再次触发 `ExitRequested`，缺少放行分支会导致永远无法退出。）
//! - 退入门闩**先于**读快照：`stop_accepting()` 具 barrier 语义，空闲路径同样必须经过。
//! - listener 停止与 tray 移除都留在 `Draining`；全部完成后才切 `Exiting` 并立即 `exit(0)`。
//! - 所有副作用幂等、可重入安全。

use std::sync::Mutex;
use std::time::Duration;

use crate::service::{LocalServiceController, TransferActivityProvider, TransferController, TransferSnapshot};

/// 退出流程所处阶段（§7.2.1）
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ExitState {
    /// 正常运行 —— 拦截 exit
    #[default]
    Running,
    /// 已弹确认，等待用户选择 —— 拦截 exit
    Confirming,
    /// 正在停接收 / 取消 / 等待 / 持久化 / 清理 —— 拦截 exit
    Draining,
    /// 已放行，等待进程结束 —— **放行** exit
    Exiting,
}

/// 退出请求的来源，仅用于日志
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QuitSource {
    Menu,
    Shortcut,
    AppMenu,
    System,
}

impl QuitSource {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Menu => "menu",
            Self::Shortcut => "shortcut",
            Self::AppMenu => "app_menu",
            Self::System => "system",
        }
    }
}

/// dring 结束后的最终清理（§7.2.2 第 3.4、3.5 步与第 4 步）
pub trait FinalCleanup: Send + Sync + 'static {
    /// 停止 HTTP listener 并释放端口
    fn stop_listener(&self);
    /// 移除常驻图标
    fn remove_tray(&self);
    /// 退出进程；调用时状态必须已是 `Exiting`
    fn exit(&self, code: i32);
}

/// 本轮退出的结果报告（§7.2.3 决定提示文案）
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ExitReport {
    /// 可取消任务是否在超时内 join 完成
    pub joined: bool,
    /// 是否仍卡在不可中断的原子提交临界区（`Renaming`）
    pub atomic_blocked: bool,
    /// 本次 drain 是否已落检查点
    pub checkpointed: bool,
}

impl ExitReport {
    /// 15 s 超时后的提示文案——按**事实**二分，不得混用（§7.2.3）。
    ///
    /// - 处于不可中断的原子提交临界区：**无论**可取消任务是否已 join 完成，都不能强退；
    /// - 否则若 15 s 内未能 join 完可取消任务（检查点已落）：可安全退出。
    pub fn timeout_message(&self) -> Option<&'static str> {
        if self.atomic_blocked {
            return Some("正在完成不可中断的提交，暂不能强制退出");
        }
        if !self.joined {
            return Some("进度已保存，可安全退出");
        }
        None
    }
}

/// `request_quit` 的结局
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QuitStep {
    /// 已在退出流程中：本次请求只等待，不重复执行副作用（§7.2.4）
    AlreadyInProgress,
    /// 用户取消；准入已恢复，状态回到 `Running`
    Cancelled,
    /// 需要用户确认。调用方展示确认 UI 后，把用户选择交给 `after_confirm` 继续。
    NeedsConfirm(TransferSnapshot),
    /// 清理全部完成，已切 `Exiting` 并调用 `exit(0)`
    Exited(ExitReport),
    /// 仍在不可中断的原子提交：**未**完成清理，调用方应继续等待并提示
    AtomicCommitPending(ExitReport),
}

/// 退出协调器
pub struct ExitCoordinator {
    state: Mutex<ExitState>,
    drain_timeout: Duration,
}

impl Default for ExitCoordinator {
    fn default() -> Self {
        Self::new(Duration::from_secs(15))
    }
}

impl ExitCoordinator {
    pub fn new(drain_timeout: Duration) -> Self {
        Self {
            state: Mutex::new(ExitState::Running),
            drain_timeout,
        }
    }

    pub fn state(&self) -> ExitState {
        *self.state.lock().unwrap()
    }

    /// §7.2.1：仅 `Exiting` 放行 `ExitRequested`。
    pub fn should_prevent_exit(&self) -> bool {
        self.state() != ExitState::Exiting
    }

    /// **阶段一**（§7.2.2 第 0–2 步）：退入门闩 → 关任务准入 → 读权威快照。
    ///
    /// 需要确认时停在 `Confirming` 并返回 [`QuitStep::NeedsConfirm`]；
    /// 调用方展示确认 UI 后必须调用 [`Self::after_confirm`] 收尾——这样可以
    /// **在等待用户作答期间不阻塞主线程**（模态对话框若同步等待会死锁事件循环）。
    pub fn begin_quit(
        &self,
        source: QuitSource,
        service: &dyn LocalServiceController,
        activity: &dyn TransferActivityProvider,
        transfer: &dyn TransferController,
        cleanup: &dyn FinalCleanup,
    ) -> QuitStep {
        log::info!("request_quit: source={}", source.as_str());

        // --- 退入门闩：只允许成功切换一次 -------------------------------
        {
            let mut st = self.state.lock().unwrap();
            match *st {
                ExitState::Running => {}
                // 已在确认 / drain / 已放行：只等待，不重复副作用
                ExitState::Confirming | ExitState::Draining | ExitState::Exiting => {
                    return QuitStep::AlreadyInProgress
                }
            }
            *st = ExitState::Confirming;
        }

        // 第 0 步：先关准入（barrier），之后才读权威快照。
        // 空闲路径同样必须经过本步——否则快照与准入之间存在新任务漏入窗口。
        service.stop_accepting();
        let snapshot = activity.snapshot();

        if snapshot.requires_confirm() {
            // 停在 Confirming，等调用方把用户选择送回来
            return QuitStep::NeedsConfirm(snapshot);
        }
        self.drain_and_exit(activity, transfer, cleanup)
    }

    /// **阶段二**：用户作答后继续。
    ///
    /// `confirmed == false` 时恢复准入并回到 `Running`（§7.2.2 第 0 步的取消分支）。
    pub fn after_confirm(
        &self,
        confirmed: bool,
        service: &dyn LocalServiceController,
        activity: &dyn TransferActivityProvider,
        transfer: &dyn TransferController,
        cleanup: &dyn FinalCleanup,
    ) -> QuitStep {
        if !confirmed {
            // 用户取消：恢复准入并回到 Running；恢复失败由实现方进入可见错误状态
            service.resume_accepting();
            *self.state.lock().unwrap() = ExitState::Running;
            log::info!("request_quit: cancelled by user");
            return QuitStep::Cancelled;
        }
        self.drain_and_exit(activity, transfer, cleanup)
    }

    /// 第 3–4 步：drain（checkpoint → cancel/join → 等原子临界区）→ 清理 → 退出。
    fn drain_and_exit(
        &self,
        activity: &dyn TransferActivityProvider,
        transfer: &dyn TransferController,
        cleanup: &dyn FinalCleanup,
    ) -> QuitStep {
        // --- 第 3 步：Draining（此后不放过任何 exit）-------------------
        *self.state.lock().unwrap() = ExitState::Draining;

        // 3.1 先落 checkpoint —— 必须在任何等待之前
        transfer.checkpoint();
        let checkpointed = activity.snapshot().checkpointed;

        // 3.2 取消 + 有界等待可取消任务（15 s 只约束这一步）
        transfer.cancel_all();
        let joined = transfer.join_to_safe_point(self.drain_timeout);

        // 3.3 是否仍卡在不可中断的原子临界区（不套用 15 s 强退上限）
        let atomic_blocked = activity.snapshot().in_atomic_critical_section();

        let report = ExitReport {
            joined,
            atomic_blocked,
            checkpointed,
        };

        if atomic_blocked {
            // 不允许在原子提交中途强退；保持 Draining，让调用方继续等待并提示
            log::warn!("request_quit: atomic commit in progress, exit deferred");
            return QuitStep::AtomicCommitPending(report);
        }

        // 3.4 停 listener（释放端口）—— 仍在 Draining
        cleanup.stop_listener();

        // 3.5 移除常驻图标 —— 仍在 Draining
        cleanup.remove_tray();

        // 第 4 步：清理全部完成后才切 Exiting，并立即 exit(0)
        *self.state.lock().unwrap() = ExitState::Exiting;
        cleanup.exit(0);

        QuitStep::Exited(report)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::service::{
        FakeLocalService, FakeTransfer, LocalServiceStatus, TransferSnapshot,
    };
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Arc;

    #[derive(Default)]
    struct RecordingCleanup {
        listener_stopped: AtomicBool,
        tray_removed: AtomicBool,
        exited: AtomicBool,
        /// exit 被调用时记录的「当时状态」，用于验证 §7.2.1 的放行
        state_at_exit: Mutex<Option<ExitState>>,
    }

    /// 用 Arc 而非借用：`FinalCleanup` 要求 `'static`
    struct TestCleanup {
        rec: Arc<RecordingCleanup>,
        coord: Arc<ExitCoordinator>,
    }

    impl FinalCleanup for TestCleanup {
        fn stop_listener(&self) {
            self.rec.listener_stopped.store(true, Ordering::SeqCst);
        }
        fn remove_tray(&self) {
            self.rec.tray_removed.store(true, Ordering::SeqCst);
        }
        fn exit(&self, _code: i32) {
            *self.rec.state_at_exit.lock().unwrap() = Some(self.coord.state());
            self.rec.exited.store(true, Ordering::SeqCst);
        }
    }

    fn running_service() -> FakeLocalService {
        FakeLocalService::new(LocalServiceStatus::Running { bound_port: 9876 })
    }

    /// A13 + §7.2.1：空闲退出不弹确认，且 exit 时状态已是 Exiting
    #[test]
    fn idle_quit_skips_confirm_and_exits_state_is_exiting() {
        let coord = Arc::new(ExitCoordinator::default());
        let svc = running_service();
        let transfer = FakeTransfer::default();
        let rec = Arc::new(RecordingCleanup::default());
        let cleanup = TestCleanup {
            rec: rec.clone(),
            coord: coord.clone(),
        };

        // 空闲路径不得要求在确认：begin_quit 直接走到 Exited 即证明未走 NeedsConfirm
        let outcome = coord.begin_quit(QuitSource::Menu, &svc, &transfer, &transfer, &cleanup);

        assert!(matches!(outcome, QuitStep::Exited(_)));
        assert!(rec.exited.load(Ordering::SeqCst));
        // exit 被调用时状态必须已是 Exiting（否则会被自家 prevent_exit 拦住）
        assert_eq!(*rec.state_at_exit.lock().unwrap(), Some(ExitState::Exiting));
    }

    /// §7.2.2 门闩：先关准入，再读快照——空闲路径也必须关
    #[test]
    fn quit_closes_admission_even_on_idle_path() {
        let coord = Arc::new(ExitCoordinator::default());
        let svc = running_service();
        let transfer = FakeTransfer::default();
        let rec = Arc::new(RecordingCleanup::default());
        let cleanup = TestCleanup {
            rec: rec.clone(),
            coord: coord.clone(),
        };

        coord.begin_quit(QuitSource::Menu, &svc, &transfer, &transfer, &cleanup);

        assert_eq!(svc.stop_accepting_calls(), 1);
        assert!(!svc.accepting());
        assert!(
            rec.listener_stopped.load(Ordering::SeqCst),
            "清理阶段必须停止 listener"
        );
        assert!(rec.tray_removed.load(Ordering::SeqCst), "清理阶段必须移除 tray");
    }

    /// A12：忙时用户取消 → 恢复准入并回到 Running
    #[test]
    fn cancel_restores_admission_and_state() {
        let coord = Arc::new(ExitCoordinator::default());
        let svc = running_service();
        let transfer = FakeTransfer::new(TransferSnapshot {
            cancellable_count: 2,
            ..Default::default()
        });
        let rec = Arc::new(RecordingCleanup::default());
        let cleanup = TestCleanup {
            rec: rec.clone(),
            coord: coord.clone(),
        };

        // 忙时：begin_quit 停在 NeedsConfirm，等用户作答
        let step = coord.begin_quit(QuitSource::Shortcut, &svc, &transfer, &transfer, &cleanup);
        assert!(
            matches!(step, QuitStep::NeedsConfirm(_)),
            "有可取消任务时必须先要确认"
        );
        assert_eq!(coord.state(), ExitState::Confirming);

        // 用户取消
        let outcome = coord.after_confirm(false, &svc, &transfer, &transfer, &cleanup);

        assert_eq!(outcome, QuitStep::Cancelled);
        assert_eq!(coord.state(), ExitState::Running);
        assert!(svc.accepting(), "取消后必须恢复准入");
        assert!(!rec.exited.load(Ordering::SeqCst));
        assert!(!svc.is_stopped(), "取消后不应停止 listener");
    }

    /// §7.2.2 顺序：checkpoint 必须先于 cancel/join
    #[test]
    fn checkpoint_happens_before_cancel() {
        let coord = Arc::new(ExitCoordinator::default());
        let svc = running_service();
        let transfer = FakeTransfer::new(TransferSnapshot {
            cancellable_count: 1,
            ..Default::default()
        });
        let rec = Arc::new(RecordingCleanup::default());
        let cleanup = TestCleanup {
            rec: rec.clone(),
            coord: coord.clone(),
        };

        let step = coord.begin_quit(QuitSource::Menu, &svc, &transfer, &transfer, &cleanup);
        assert!(matches!(step, QuitStep::NeedsConfirm(_)));
        coord.after_confirm(true, &svc, &transfer, &transfer, &cleanup);

        assert_eq!(*transfer.checkpoint_count.lock().unwrap(), 1);
        assert_eq!(*transfer.cancel_count.lock().unwrap(), 1);
        // checkpoint 已落，报告里应体现
        // （join_succeeds 默认 true，故 joined = true）
    }

    /// §7.2.3：仍处于原子提交时**不得**完成退出
    #[test]
    fn atomic_commit_defers_exit() {
        let coord = Arc::new(ExitCoordinator::default());
        let svc = running_service();
        let transfer = FakeTransfer::new(TransferSnapshot {
            committing_count: 1,
            ..Default::default()
        });
        let rec = Arc::new(RecordingCleanup::default());
        let cleanup = TestCleanup {
            rec: rec.clone(),
            coord: coord.clone(),
        };

        let step = coord.begin_quit(QuitSource::Menu, &svc, &transfer, &transfer, &cleanup);
        assert!(matches!(step, QuitStep::NeedsConfirm(_)));
        let outcome = coord.after_confirm(true, &svc, &transfer, &transfer, &cleanup);

        match outcome {
            QuitStep::AtomicCommitPending(report) => {
                assert!(report.atomic_blocked);
                assert_eq!(
                    report.timeout_message(),
                    Some("正在完成不可中断的提交，暂不能强制退出")
                );
            }
            other => panic!("expected AtomicCommitPending, got {other:?}"),
        }
        assert!(!rec.exited.load(Ordering::SeqCst));
        assert!(!rec.tray_removed.load(Ordering::SeqCst), "尚未清理");
    }

    /// A20 的判定部分：Draining 期间必须继续拦截 exit
    #[test]
    fn draining_still_prevents_exit() {
        let coord = Arc::new(ExitCoordinator::new(Duration::from_secs(0)));
        let svc = running_service();
        // 制造「join 超时但已过临界区」→ 允许退出，但报告应给「已保存」文案
        let transfer = FakeTransfer::new(TransferSnapshot {
            cancellable_count: 1,
            ..Default::default()
        });
        transfer.join_succeeds.store(false, Ordering::SeqCst);
        let rec = Arc::new(RecordingCleanup::default());
        let cleanup = TestCleanup {
            rec: rec.clone(),
            coord: coord.clone(),
        };

        let step = coord.begin_quit(QuitSource::Menu, &svc, &transfer, &transfer, &cleanup);
        assert!(matches!(step, QuitStep::NeedsConfirm(_)));
        let outcome = coord.after_confirm(true, &svc, &transfer, &transfer, &cleanup);

        match outcome {
            QuitStep::Exited(report) => {
                assert!(!report.joined);
                assert!(!report.atomic_blocked);
                assert_eq!(report.timeout_message(), Some("进度已保存，可安全退出"));
            }
            other => panic!("expected Exited, got {other:?}"),
        }
    }

    /// A14 / §7.2.4：重复退出请求不重复执行副作用
    #[test]
    fn repeated_quit_is_idempotent() {
        let coord = Arc::new(ExitCoordinator::default());
        let svc = running_service();
        let transfer = FakeTransfer::default();
        let rec = Arc::new(RecordingCleanup::default());

        // 第一次：正常退出
        {
            let cleanup = TestCleanup {
                rec: rec.clone(),
                coord: coord.clone(),
            };
            let outcome =
                coord.begin_quit(QuitSource::Menu, &svc, &transfer, &transfer, &cleanup);
            assert!(matches!(outcome, QuitStep::Exited(_)));
        }

        // 第二次（模拟 exit() 再次触发 ExitRequested）：只应返回 AlreadyInProgress
        let cleanup = TestCleanup {
            rec: rec.clone(),
            coord: coord.clone(),
        };
        let outcome = coord.begin_quit(QuitSource::System, &svc, &transfer, &transfer, &cleanup);
        assert_eq!(outcome, QuitStep::AlreadyInProgress);
        assert_eq!(svc.stop_accepting_calls(), 1, "门闩只允许成功切换一次");
        assert_eq!(*transfer.cancel_count.lock().unwrap(), 1, "不得重复 drain");
    }

    /// §7.2.1：仅 Exiting 放行
    #[test]
    fn only_exiting_allows_exit() {
        let coord = Arc::new(ExitCoordinator::default());
        assert!(coord.should_prevent_exit());
        *coord.state.lock().unwrap() = ExitState::Confirming;
        assert!(coord.should_prevent_exit());
        *coord.state.lock().unwrap() = ExitState::Draining;
        assert!(coord.should_prevent_exit());
        *coord.state.lock().unwrap() = ExitState::Exiting;
        assert!(!coord.should_prevent_exit());
    }
}
