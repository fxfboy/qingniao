//! 本地 HTTP 服务（D7/D14/D20，设计文档 §10）：127.0.0.1 双路由 loopback 服务。
//!
//! wire contract（§10.1）：
//! - 仅监听 `127.0.0.1:<bound_port>`；Host 校验防 DNS rebinding
//! - GET  /dl?t=…       无副作用，校验通过 → 渲染确认页（一次性 handle，活到链接新鲜度窗口结束）
//! - POST /dl/confirm   Origin/Referer 校验（缺/跨 → 403）→ Content-Type → handle 单消费
//! - 错误统一 JSON `{"code":..,"msg":..}`；Cache-Control: no-store
//! - 并发连接 ≤ 4（4 个 worker 线程共同 recv）；读超时 10 s、写超时 30 s
//! - 退出门闩（对齐 dock-tray A19）：stop_accepting 后新请求一律 503

use crate::service::{LocalServiceController, LocalServiceStatus, ServiceStatusProvider, StatusCell};
use crate::transfer::engine::{ClaimError, Engine};
use std::io::Read;
use std::net::TcpListener;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use tiny_http::{Header, Method, Response, Server};

pub struct RealLocalService {
    /// 权威状态持有者（与 AppState.set_service_status 共享）
    cell: Arc<StatusCell>,
    inner: Mutex<Option<ServerHandle>>,
    /// 准入旗标（stop_accepting/resume 语义；暴露给引擎适配器构造快照）
    accepting: Arc<AtomicBool>,
    /// 当前实际绑定端口（stop 时用于唤醒阻塞的 recv）
    bound: std::sync::atomic::AtomicU16,
    /// 引擎句柄（payload 校验 / 会话 / 下载执行）
    engine: Arc<Engine>,
}

struct ServerHandle {
    server: Arc<Server>,
    stop: Arc<AtomicBool>,
    workers: Vec<std::thread::JoinHandle<()>>,
}

/// worker 线程上下文
struct Ctx {
    server: Arc<Server>,
    stop: Arc<AtomicBool>,
    accepting: Arc<AtomicBool>,
    engine: Arc<Engine>,
    bound_port: u16,
    page: &'static str,
}

impl RealLocalService {
    pub fn new(cell: Arc<StatusCell>, engine: Arc<Engine>) -> Arc<Self> {
        Arc::new(Self {
            cell,
            inner: Mutex::new(None),
            accepting: Arc::new(AtomicBool::new(true)),
            bound: std::sync::atomic::AtomicU16::new(0),
            engine,
        })
    }

    /// 准入旗标（引擎适配器构造 TransferSnapshot 用）
    pub fn accepting_flag(&self) -> Arc<AtomicBool> {
        self.accepting.clone()
    }

    /// 暴露引擎（命令层用）
    pub fn engine(&self) -> Arc<Engine> {
        self.engine.clone()
    }

    /// 绑定并启动 worker 线程。端口占用 → Failed{PortInUse}，不自动漂移（§10.2）
    pub fn start(self: &Arc<Self>, port: u16, page: &'static str) -> LocalServiceStatus {
        let listener = match TcpListener::bind(("127.0.0.1", port)) {
            Ok(l) => l,
            Err(e) => {
                log::warn!("本地服务绑定 {port} 失败: {e}");
                let st = LocalServiceStatus::Failed {
                    kind: crate::service::ServiceFailure::PortInUse,
                    requested_port: port,
                };
                self.cell.set(st.clone());
                return st;
            }
        };
        let server = match Server::from_listener(listener, None) {
            Ok(s) => Arc::new(s),
            Err(e) => {
                log::error!("tiny_http 初始化失败: {e}");
                let st = LocalServiceStatus::Failed {
                    kind: crate::service::ServiceFailure::PortInUse,
                    requested_port: port,
                };
                self.cell.set(st.clone());
                return st;
            }
        };
        self.bound.store(port, Ordering::SeqCst);
        let stop = Arc::new(AtomicBool::new(false));
        let mut workers = Vec::new();
        for i in 0..4 {
            let ctx = Ctx {
                server: server.clone(),
                stop: stop.clone(),
                accepting: self.accepting.clone(),
                engine: self.engine.clone(),
                bound_port: port,
                page,
            };
            workers.push(
                std::thread::Builder::new()
                    .name(format!("qn-local-{i}"))
                    .spawn(move || worker_loop(ctx))
                    .expect("spawn worker"),
            );
        }
        self.accepting.store(true, Ordering::SeqCst);
        let st = LocalServiceStatus::Running { bound_port: port };
        self.cell.set(st.clone());
        log::info!("本地服务已启动：127.0.0.1:{port}");
        self.inner.lock().unwrap().replace(ServerHandle { server, stop, workers });
        st
    }

    pub fn is_running(&self) -> bool {
        self.inner.lock().unwrap().is_some()
    }
}

impl ServiceStatusProvider for RealLocalService {
    fn snapshot(&self) -> LocalServiceStatus {
        self.cell.snapshot()
    }
}

impl LocalServiceController for RealLocalService {
    /// barrier 语义（§10.1 退出门闩）：返回时新请求一律 503
    fn stop_accepting(&self) {
        self.accepting.store(false, Ordering::SeqCst);
    }
    fn resume_accepting(&self) {
        self.accepting.store(true, Ordering::SeqCst);
    }
    fn stop(&self) {
        let port = self.bound.load(Ordering::SeqCst);
        let handle = self.inner.lock().unwrap().take();
        if let Some(h) = handle {
            h.stop.store(true, Ordering::SeqCst);
            // 唤醒阻塞在 recv() 的 worker（unblock 使 recv 返回 Err → worker 退出）。
            //
            // **必须每个 worker 唤醒一次**：tiny_http 的 `MessagesQueue::unblock()`
            // 只往队列 push 一个 `Control::Unblock` 并 `notify_one()`——一次调用
            // 只能唤醒**一个**阻塞在 `recv()` 的线程（见 tiny_http 文档原文
            // "If there are several such threads, only one is unblocked"）。
            // 这里 4 个 worker 全部阻塞在 `recv()`，只调用一次会留下 3 个永远
            // 阻塞，下面的 `join()` 将卡死在第一个未被唤醒的 worker 上——
            // 主线程随之冻结，退出协议走不到 `remove_tray` / `exit(0)`，
            // 进程变成僵尸（托盘卡死、下次启动被单实例插件挡住）。
            // 多出的 Unblock 信号无害：忙于处理请求的 worker 完事后看到 stop
            // 旗标自行退出，不消费信号；Server 随 handle 一起销毁，队列不复用。
            for _ in 0..h.workers.len() {
                h.server.unblock();
            }
            for w in h.workers {
                let _ = w.join();
            }
        }
        let _ = port;
        self.cell.set(LocalServiceStatus::Stopped);
    }
    /// 端口变更（§10.2）：统一走 [`restart_service`]（需要页面资源）
    fn restart(&self, _port: u16) {}
}

/// 真实重启入口（LocalServiceController::restart 需要页面资源，由 lib.rs 装配后调用）
pub fn restart_service(svc: &Arc<RealLocalService>, port: u16, page: &'static str) -> LocalServiceStatus {
    svc.stop();
    svc.start(port, page)
}

fn worker_loop(ctx: Ctx) {
    loop {
        if ctx.stop.load(Ordering::SeqCst) {
            break;
        }
        let request = match ctx.server.recv() {
            Ok(r) => r,
            Err(_) => break,
        };
        handle_request(&ctx, request);
    }
}

fn handle_request(ctx: &Ctx, request: tiny_http::Request) {
    // 读超时限制：tiny_http 无 per-request 超时，靠请求体上限保护
    let method = request.method().clone();
    let url = request.url().to_string();
    let host = header_value(&request, "Host");

    // Host 校验（防 DNS rebinding，§10.1）
    if host != format!("127.0.0.1:{}", ctx.bound_port) {
        respond_json(request, 403, &serde_json::json!({"code": 403000, "msg": "Host 不合法"}));
        return;
    }
    // 退出门闩：停止接受后一律 503（对齐 dock-tray A19）
    if !ctx.accepting.load(Ordering::SeqCst) {
        respond_json(request, 503, &serde_json::json!({"code": 503000, "msg": "服务停止中"}));
        return;
    }

    match (&method, url.split('?').next().unwrap_or("")) {
        (Method::Get, "/dl") => handle_get_dl(ctx, request, &url),
        (Method::Post, "/dl/confirm") => handle_post_confirm(ctx, request),
        _ => {
            respond_json(request, 404, &serde_json::json!({"code": 404000, "msg": "未找到"}));
        }
    }
}

fn header_value(request: &tiny_http::Request, name: &str) -> String {
    request
        .headers()
        .iter()
        .find(|h| h.field.as_str().as_str().eq_ignore_ascii_case(name))
        .map(|h| h.value.as_str().to_string())
        .unwrap_or_default()
}

/* ===================== GET /dl ===================== */

fn handle_get_dl(ctx: &Ctx, request: tiny_http::Request, url: &str) {
    let payload = match query_param(url, "t") {
        Some(t) if !t.is_empty() => t,
        _ => {
            respond_json(request, 410, &serde_json::json!({"code": 410000, "msg": "链接缺少载荷"}));
            return;
        }
    };
    // 统一渲染：所有占位符必须被替换（否则原样露出模板痕迹）
    let mut vars: Vec<(&str, String)> = vec![
        ("{{PORT}}", ctx.bound_port.to_string()),
        ("{{NAME}}", String::new()),
        ("{{SIZE_HUMAN}}", String::new()),
        ("{{SENT_TIME}}", String::new()),
        ("{{TTL_TEXT}}", String::new()),
        ("{{TTL_SECS}}", "0".into()),
        // 链接有效期文案（与「链接 N 分钟内有效」「剩余有效期」倒计时同一常量）
        ("{{FRESH_WINDOW}}", format!("{} 分钟", crate::transfer::crypto::FRESHNESS_WINDOW_MINUTES)),
        ("{{HANDLE}}", String::new()),
        ("{{DIR}}", String::new()),
        ("{{FINAL_PATH}}", String::new()),
        ("{{CONSUMED_AT}}", String::new()),
        ("{{ERROR_MSG}}", String::new()),
        ("{{ELAPSED}}", String::new()),
    ];
    let state: String;
    let mut status = 200u16;
    match ctx.engine.evaluate_payload(&payload) {
        Ok(ev) => {
            if let Some(at) = ctx.engine.consumed_at(&ev.fingerprint) {
                // 已消费 → 「已下载过」页（重放抑制，§9.4）
                state = "consumed".into();
                set_var(&mut vars, "{{FINAL_PATH}}", ctx.engine.consumed_path_of(&ev.fingerprint).unwrap_or_default());
                set_var(&mut vars, "{{CONSUMED_AT}}", format_ts(at));
                let html = render_page(ctx.page, &state, &vars);
                respond_html(request, 200, &html);
                return;
            }
            // 创建一次性会话（handle = CSPRNG 128-bit hex，活到链接新鲜度窗口结束）
            match ctx.engine.create_session(&payload, ev) {
                Ok(sess) => {
                    // 倒计时 = 链接剩余有效期（payload.ts + 30 min），与群消息/历史文案同源
                    let ttl = (sess.expires_at - crate::transfer::crypto::now_unix()).max(0);
                    state = "pending".into();
                    set_var(&mut vars, "{{NAME}}", html_escape(&sess.name));
                    set_var(&mut vars, "{{SIZE_HUMAN}}", human_size(sess.size));
                    set_var(&mut vars, "{{SENT_TIME}}", format_ts(sess.created_at));
                    set_var(&mut vars, "{{TTL_TEXT}}", format!("{}:{:02}", ttl / 60, ttl % 60));
                    set_var(&mut vars, "{{TTL_SECS}}", ttl.to_string());
                    set_var(&mut vars, "{{HANDLE}}", sess.handle);
                    set_var(&mut vars, "{{DIR}}", html_escape(&sess.download_dir));
                }
                Err(msg) => {
                    // 密钥不匹配 / 元数据非法等（§10.1：payload 非法 → 410）
                    respond_json(request, 410, &serde_json::json!({"code": 410001, "msg": msg}));
                    return;
                }
            }
        }
        Err(msg) => {
            // kid 无匹配 → 密钥不匹配页；ts 过期 → 过期页（§10.1：payload 非法/过期 → 410）
            state = if msg.contains("密钥") { "kid".into() } else { "expired".into() };
            status = 410;
        }
    }
    let html = render_page(ctx.page, &state, &vars);
    respond_html(request, status, &html);
}

fn set_var(vars: &mut Vec<(&'static str, String)>, key: &'static str, val: String) {
    for (k, v) in vars.iter_mut() {
        if *k == key {
            *v = val;
            return;
        }
    }
}

fn render_page(page: &str, state: &str, vars: &[(&str, String)]) -> String {
    let mut html = page.to_string();
    html = html.replace("{{STATE}}", state);
    for (k, v) in vars {
        html = html.replace(k, v);
    }
    html
}

/* ===================== POST /dl/confirm ===================== */

fn handle_post_confirm(ctx: &Ctx, mut request: tiny_http::Request) {
    // ① Origin / Referer 校验（缺或跨 Origin → 403，防跨站 POST）
    let expect = format!("http://127.0.0.1:{}", ctx.bound_port);
    let origin = header_value(&request, "Origin");
    let referer = header_value(&request, "Referer");
    let origin_ok = if origin.is_empty() {
        referer.starts_with(&expect)
    } else {
        origin == expect
    };
    if !origin_ok {
        respond_json(request, 403, &serde_json::json!({"code": 403001, "msg": "来源校验失败"}));
        return;
    }
    // ② Content-Type: application/json
    let ctype = header_value(&request, "Content-Type");
    if !ctype.to_ascii_lowercase().starts_with("application/json") {
        respond_json(request, 400, &serde_json::json!({"code": 400001, "msg": "Content-Type 必须为 application/json"}));
        return;
    }
    // ③ 请求体 ≤ 1 KB
    let mut body = String::new();
    let mut limited = request.as_reader().take(1025);
    if limited.read_to_string(&mut body).is_err() || body.len() > 1024 {
        respond_json(request, 400, &serde_json::json!({"code": 400002, "msg": "请求体不合法"}));
        return;
    }
    let handle = serde_json::from_str::<serde_json::Value>(&body)
        .ok()
        .and_then(|v| v.get("handle").and_then(|h| h.as_str()).map(String::from));
    let Some(handle) = handle else {
        respond_json(request, 400, &serde_json::json!({"code": 400003, "msg": "handle 缺失"}));
        return;
    };

    // 校验顺序 ③：handle 存在、未过期 → 原子标记「消费中」→ 执行下载 + 删除
    match ctx.engine.claim_session(&handle) {
        Ok(crate::transfer::engine::ClaimResult::AlreadyDone { path }) => {
            // 幂等：已完成 → 200 + 结果摘要，不重复下载/删除（§9.4）
            respond_json(request, 200, &serde_json::json!({
                "code": 0,
                "replay": true,
                "final_path": path,
            }));
        }
        Ok(crate::transfer::engine::ClaimResult::Start(pending)) => {
            match ctx.engine.run_download_sync(&pending) {
                Ok(final_path) => {
                    respond_json(request, 200, &serde_json::json!({
                        "code": 0,
                        "final_path": final_path,
                    }));
                }
                Err(msg) => {
                    respond_json(request, 500, &serde_json::json!({"code": 500001, "msg": msg}));
                }
            }
        }
        Err(ClaimError::Busy) => {
            respond_json(request, 409, &serde_json::json!({"code": 409000, "msg": "正在下载"}));
        }
        Err(ClaimError::Expired) => {
            respond_json(request, 410, &serde_json::json!({"code": 410002, "msg": "确认已过期，请重新打开链接"}));
        }
        Err(ClaimError::NotFound) => {
            respond_json(request, 400, &serde_json::json!({"code": 400004, "msg": "handle 非法或已使用"}));
        }
    }
}

/* ===================== 响应工具 ===================== */

fn respond_json(request: tiny_http::Request, status: u16, body: &serde_json::Value) {
    let data = body.to_string();
    let mut resp = Response::from_string(data).with_status_code(status);
    for h in [
        Header::from_bytes(&b"Content-Type"[..], &b"application/json; charset=utf-8"[..]).unwrap(),
        Header::from_bytes(&b"Cache-Control"[..], &b"no-store"[..]).unwrap(),
        Header::from_bytes(&b"Referrer-Policy"[..], &b"no-referrer"[..]).unwrap(),
    ] {
        resp.add_header(h);
    }
    let _ = request.respond(resp);
}

fn respond_html(request: tiny_http::Request, status: u16, html: &str) {
    let mut resp = Response::from_string(html.to_string()).with_status_code(status);
    for h in [
        Header::from_bytes(&b"Content-Type"[..], &b"text/html; charset=utf-8"[..]).unwrap(),
        Header::from_bytes(&b"Cache-Control"[..], &b"no-store"[..]).unwrap(),
        Header::from_bytes(&b"Referrer-Policy"[..], &b"no-referrer"[..]).unwrap(),
    ] {
        resp.add_header(h);
    }
    let _ = request.respond(resp);
}

fn query_param(url: &str, key: &str) -> Option<String> {
    let query = url.split_once('?')?.1;
    for pair in query.split('&') {
        let (k, v) = pair.split_once('=')?;
        if k == key {
            return Some(urldecode(v));
        }
    }
    None
}

fn urldecode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'%' if i + 2 < bytes.len() + 1 && i + 2 < bytes.len() + 1 => {
                if i + 2 < bytes.len() {
                    let hi = (bytes[i + 1] as char).to_digit(16);
                    let lo = (bytes[i + 2] as char).to_digit(16);
                    if let (Some(h), Some(l)) = (hi, lo) {
                        out.push((h * 16 + l) as u8);
                        i += 3;
                        continue;
                    }
                }
                out.push(bytes[i]);
                i += 1;
            }
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            b => {
                out.push(b);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).to_string()
}

fn html_escape(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;")
}

fn human_size(b: u64) -> String {
    if b >= 1024 * 1024 * 1024 { format!("{:.2} GB", b as f64 / (1024.0 * 1024.0 * 1024.0)) }
    else if b >= 1024 * 1024 { format!("{:.1} MB", b as f64 / (1024.0 * 1024.0)) }
    else if b >= 1024 { format!("{} KB", b / 1024) }
    else { format!("{b} B") }
}

fn format_ts(unix: i64) -> String {
    match time::OffsetDateTime::from_unix_timestamp(unix) {
        Ok(t) => format!("{}月{}日 {:02}:{:02}", t.month() as u8, t.day(), t.hour(), t.minute()),
        Err(_) => String::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn query_param_extracts_t() {
        assert_eq!(query_param("/dl?t=abc123", "t"), Some("abc123".into()));
        assert_eq!(query_param("/dl?t=abc&x=1", "t"), Some("abc".into()));
        assert_eq!(query_param("/dl", "t"), None);
    }

    #[test]
    fn urldecode_decodes_percent_and_plus() {
        assert_eq!(urldecode("a%20b+c"), "a b c");
        assert_eq!(urldecode("%E4%B8%AD"), "中");
    }

    #[test]
    fn html_escape_escapes_all() {
        assert_eq!(html_escape("<b>&\"x\"</b>"), "&lt;b&gt;&amp;&quot;x&quot;&lt;/b&gt;");
    }
}
