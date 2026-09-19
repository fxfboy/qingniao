//! 常驻托盘 / 菜单栏图标与菜单
//!
//! 对应设计文档 §5.1（菜单项契约与稳定 ID）、§5.2（状态与事件契约）、§5.3（平台交互差异）。
//!
//! 关键约定：
//! - 菜单事件**一律按稳定 `MenuId` 分派**，不得用展示文字分派（状态项文字含动态端口）。
//! - `service_status` 是 **disabled 只读项**；`open_service_settings` 在阶段 B 启用前亦为 disabled。
//! - 左键行为按平台区分：macOS 单击弹菜单（原生习惯），Windows 左键恢复窗口、右键弹菜单。
//! - 菜单文案的端口只来自状态携带的字段，禁止硬编码默认端口。

use tauri::menu::{MenuBuilder, MenuItem, MenuItemBuilder};
use tauri::tray::TrayIconBuilder;
#[cfg(target_os = "macos")]
use tauri::menu::Menu;
#[cfg(not(target_os = "macos"))]
use tauri::tray::{MouseButton, MouseButtonState, TrayIconEvent};
use tauri::{AppHandle, Manager, Wry};

use crate::service::ServiceStatusProvider;
use crate::AppState;

/// 托盘唯一 id（§8.1：仅主实例在 setup 中创建，全应用只有一个）
pub const TRAY_ID: &str = "qingniao-tray";

// ---- 菜单稳定 ID（§5.1）----
pub const ID_OPEN_MAIN: &str = "open_main";
pub const ID_SERVICE_STATUS: &str = "service_status";
pub const ID_OPEN_DOWNLOADS: &str = "open_downloads";
pub const ID_OPEN_SERVICE_SETTINGS: &str = "open_service_settings";
pub const ID_QUIT: &str = "quit";

/// macOS 应用菜单里的 Quit 项 id。
/// 与托盘菜单的 `ID_QUIT` 分开命名（来源不同），但语义相同：都走统一退出协议。
pub const ID_APP_QUIT: &str = "app_quit";

/// 构造 macOS 应用菜单。
///
/// **为什么必须自建**：macOS 的 ⌘Q 默认绑定在系统**预定义** Quit 项上，它走
/// NSApplication `terminate:`，**不会**产生 `RunEvent::ExitRequested`（已在 dev 模式下
/// 实测确认：按 ⌘Q 后日志零新增、进程直接消失）。因此预定义 Quit 会完全绕过 §7.2 的退出协议——
/// 不落 checkpoint、不 drain、不做二次确认。
///
/// 把预定义 Quit 换成我们自己的菜单项后，⌘Q 才会进入 `request_quit`，
/// 与托盘「退出青鸟」同语义（§7.1）。
///
/// 其余项沿用系统预定义（编辑菜单必须保留，否则输入框会失去 ⌘C/⌘V/⌘A）。
#[cfg(target_os = "macos")]
pub fn build_app_menu(app: &AppHandle) -> tauri::Result<Menu<Wry>> {
    use tauri::menu::{PredefinedMenuItem, SubmenuBuilder};

    let app_submenu = SubmenuBuilder::new(app, "青鸟")
        .item(&PredefinedMenuItem::about(app, Some("关于青鸟"), None)?)
        .separator()
        .item(&PredefinedMenuItem::services(app, None)?)
        .separator()
        .item(&PredefinedMenuItem::hide(app, Some("隐藏青鸟"))?)
        .item(&PredefinedMenuItem::hide_others(app, Some("隐藏其他"))?)
        .item(&PredefinedMenuItem::show_all(app, Some("全部显示"))?)
        .separator()
        // 关键：自建 Quit，使 ⌘Q 进入统一退出协议
        .item(
            &MenuItemBuilder::with_id(ID_APP_QUIT, "退出青鸟")
                .accelerator("Cmd+Q")
                .build(app)?,
        )
        .build()?;

    let edit_submenu = SubmenuBuilder::new(app, "编辑")
        .item(&PredefinedMenuItem::undo(app, None)?)
        .item(&PredefinedMenuItem::redo(app, None)?)
        .separator()
        .item(&PredefinedMenuItem::cut(app, None)?)
        .item(&PredefinedMenuItem::copy(app, None)?)
        .item(&PredefinedMenuItem::paste(app, None)?)
        .item(&PredefinedMenuItem::select_all(app, None)?)
        .build()?;

    let window_submenu = SubmenuBuilder::new(app, "窗口")
        .item(&PredefinedMenuItem::minimize(app, None)?)
        .item(&PredefinedMenuItem::close_window(app, None)?)
        .separator()
        .item(&PredefinedMenuItem::bring_all_to_front(app, None)?)
        .build()?;

    MenuBuilder::new(app)
        .item(&app_submenu)
        .item(&edit_submenu)
        .item(&window_submenu)
        .build()
}

/// 需要保留句柄以便运行时更新的菜单项（§5.2）
pub struct TrayHandles {
    pub status_item: MenuItem<Wry>,
}

/// 状态项刷新的抽象。
///
/// **为什么需要这个缝**：`TrayHandles` 依赖真实的 `MenuItem`，脱离 Tauri 无法构造，
/// 于是「状态迁移 → 菜单更新」这条链路在单测里无从验证——A8 的验收环境恰恰是
/// `unit/fake`。把「写一行状态文字」抽成接口后，测试可以注入记录用的替身。
pub trait StatusDisplay: Send + Sync + 'static {
    fn show_status(&self, label: &str);
}

impl StatusDisplay for TrayHandles {
    fn show_status(&self, label: &str) {
        if let Err(e) = self.status_item.set_text(label) {
            log::warn!("刷新本地服务状态菜单失败: {e}");
        }
    }
}

/// 在 `setup` 中构造**唯一**一个 tray。
///
/// 注意：必须在 single-instance 插件完成主实例判定之后调用（§8.1）；
/// 且**不得**在 `tauri.conf.json` 配置 `app.trayIcon`，否则 Tauri 会在
/// `initialize_plugins()` 之前自动创建第二个 tray（§13.1 事实 15）。
pub fn build_tray(app: &AppHandle) -> tauri::Result<TrayHandles> {
    let status = app.state::<AppState>().service.read().unwrap().status.snapshot();

    // 状态项：disabled 只读，承担可观察性（§5.1）
    let status_item = MenuItemBuilder::with_id(ID_SERVICE_STATUS, status.menu_label())
        .enabled(false)
        .build(app)?;

    // 布局：打开主界面 → 分隔 → 状态 + 下载目录 + 服务设置 → 分隔 → 退出
    let menu = MenuBuilder::new(app)
        .item(&MenuItemBuilder::with_id(ID_OPEN_MAIN, "打开主界面").build(app)?)
        .separator()
        .item(&status_item)
        .item(&MenuItemBuilder::with_id(ID_OPEN_DOWNLOADS, "打开下载目录").build(app)?)
        .item(
            // 阶段 B 交付配置页后再启用（§5.1、§15.0）
            &MenuItemBuilder::with_id(ID_OPEN_SERVICE_SETTINGS, "本地服务设置…")
                .enabled(false)
                .build(app)?,
        )
        .separator()
        .item(&MenuItemBuilder::with_id(ID_QUIT, "退出青鸟").build(app)?)
        .build()?;

    // 左键行为按平台区分（仿 ORCA `src/main/tray/system-tray.ts`）：
    // - macOS：**单击直接弹菜单**——这是菜单栏图标的原生习惯；恢复窗口由 Dock 图标
    //   承担（`RunEvent::Reopen`），不需要常驻图标再重复一份。
    // - Windows：**左键恢复窗口、右键弹菜单**——通知区域的常规手势。
    #[cfg(target_os = "macos")]
    let show_menu_on_left_click = true;
    #[cfg(not(target_os = "macos"))]
    let show_menu_on_left_click = false;

    // 菜单事件统一由 `Builder::on_menu_event` 全局分派（托盘菜单与窗口应用菜单共用
    // 同一个入口，见 lib.rs 的 `menu_action`），这里不再单独注册，避免重复处理。
    let mut builder = TrayIconBuilder::with_id(TRAY_ID)
        .menu(&menu)
        .tooltip("青鸟 · 飞书消息助手")
        .show_menu_on_left_click(show_menu_on_left_click);

    // 只有 Windows 才把左键单击用于恢复主窗口
    #[cfg(not(target_os = "macos"))]
    {
        builder = builder.on_tray_icon_event(|tray, event| {
            if let TrayIconEvent::Click {
                button: MouseButton::Left,
                button_state: MouseButtonState::Up,
                ..
            } = event
            {
                crate::show_main_window(tray.app_handle());
            }
        });
    }

    // 常驻图标：按平台分别提供（仿 ORCA `src/main/tray/system-tray.ts` 的做法）
    //
    // 注意：include_image! 的路径相对 **crate root**（src-tauri/），不是源文件所在目录。
    #[cfg(target_os = "macos")]
    {
        // macOS 菜单栏：透明底 + **纯黑**字形的 template 图。
        //
        // template 语义：系统只取 alpha 通道、颜色由当前外观决定
        // （浅色菜单栏→黑笔画，深色菜单栏→自动反白为白笔画）。
        // 因此这张图**必须**是透明底、不能带实色底——
        // 若给它不透明的黑底，整块都会被涂黑，鸟形会完全消失。
        //
        // 尺寸无需手工指定：tray-icon 会把图标高度归一到 18pt 并保持宽高比
        // （见 tray-icon `platform_impl/macos/mod.rs`），
        // 与 Electron 需要手工准备 22×14 的 ORCA 不同。
        builder = builder
            .icon(tauri::include_image!("icons/tray-template.png"))
            .icon_as_template(true);
    }

    #[cfg(not(target_os = "macos"))]
    {
        // Windows 托盘：沿用**彩色**应用图标（通知区域本就显示彩色图标）。
        // 预缩到 32px：tray-icon 在 Windows 侧不做尺寸归一，
        // 直接用原图会偏大/发虚（ORCA 在 Windows 侧同样复用彩色应用图标并缩到 16px）。
        builder = builder.icon(tauri::include_image!("icons/tray-windows.png"));
    }

    builder.build(app)?;

    Ok(TrayHandles { status_item })
}

/// macOS：切换 Dock 可见性与激活策略（仿 cc-switch `tray.rs` 的 `apply_tray_policy`）。
///
/// 主窗口隐藏时切 `Accessory` 并隐藏 Dock 图标，应用退居纯托盘常驻；
/// 恢复窗口时切回 `Regular`，Dock 图标随之恢复（`RunEvent::Reopen` 也依赖它）。
/// 两个调用都允许失败（如非主线程时机），只记日志不上抛——托盘常驻不受影响。
#[cfg(target_os = "macos")]
pub fn apply_dock_policy(app: &tauri::AppHandle, dock_visible: bool) {
    use tauri::ActivationPolicy;

    let policy = if dock_visible {
        ActivationPolicy::Regular
    } else {
        ActivationPolicy::Accessory
    };

    if let Err(err) = app.set_dock_visibility(dock_visible) {
        log::warn!("设置 Dock 显示状态失败: {err}");
    }
    if let Err(err) = app.set_activation_policy(policy) {
        log::warn!("设置激活策略失败: {err}");
    }

    if dock_visible {
        restore_dock_icon();
    }
}

/// 从 `Accessory` 切回 `Regular` 后，macOS 会重建 Dock tile，
/// 图标回落为通用可执行文件图标（黑色 "exec"）。
///
/// 但 `NSApplication.applicationIcon` 属性仍持有启动时从 Info.plist
/// 加载的原图——这里把它**重新 set 一遍**，强制 Dock 重新取图。
/// 只能在主线程调用（Tauri 的窗口事件 / 托盘菜单回调都在主线程触发）。
#[cfg(target_os = "macos")]
fn restore_dock_icon() {
    use objc2::MainThreadMarker;
    use objc2_app_kit::NSApplication;

    // MainThreadMarker::new()：安全的运行时主线程判定，非主线程返回 None
    let Some(mtm) = MainThreadMarker::new() else {
        log::warn!("restore_dock_icon 不在主线程，跳过");
        return;
    };
    // sharedApplication 带有 MainThreadMarker 参数，绑定本身即安全
    let ns_app = NSApplication::sharedApplication(mtm);
    let icon = ns_app.applicationIconImage();
    // setApplicationIconImage 要求主线程，绑定标为 unsafe；由上方 mtm 保证
    unsafe { ns_app.setApplicationIconImage(icon.as_deref()) };
}
