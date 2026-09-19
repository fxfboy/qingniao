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
use qingniao_core::transfer::engine::{ClaimError, Engine};
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
        ("{{FRESH_WINDOW}}", format!("{} 分钟", qingniao_core::transfer::crypto::FRESHNESS_WINDOW_MINUTES)),
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
                    let ttl = (sess.expires_at - qingniao_core::transfer::crypto::now_unix()).max(0);
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
        Ok(qingniao_core::transfer::engine::ClaimResult::AlreadyDone { path }) => {
            // 幂等：已完成 → 200 + 结果摘要，不重复下载/删除（§9.4）
            respond_json(request, 200, &serde_json::json!({
                "code": 0,
                "replay": true,
                "final_path": path,
            }));
        }
        Ok(qingniao_core::transfer::engine::ClaimResult::Start(pending)) => {
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

/* ===================== U17：wire contract 与退出竞态（P0-4，协议 §16） ===================== */
//
// 起真实 listener + 4 worker，从原始 TCP 断言状态码与语义。Engine 用注入的
// `KeySource` / `ChunkStore`（M0a 出口 3 的接缝），不碰真实凭据库、不发外部网络请求。

#[cfg(test)]
mod wire_tests {
    use super::*;
    use qingniao_core::transfer::crypto::{self, ChunkMeta, Envelope, Metadata};
    use qingniao_core::transfer::engine::{ChunkStore, Engine, FixedHost, KeySource};
    use sha2::{Digest, Sha256};
    use std::collections::HashMap;
    use std::io::{Read, Write};
    use std::net::{TcpListener, TcpStream};
    use std::sync::mpsc;
    use std::time::Duration;

    /// 最小确认页模板：占位符被渲染后即可从 HTML 中提取 handle 与状态
    const TEST_PAGE: &str = "<html data-state=\"{{STATE}}\" handle=\"{{HANDLE}}\" dir=\"{{DIR}}\"></html>";

    struct FixedKeys;
    impl KeySource for FixedKeys {
        fn current(&self) -> Result<Option<String>, String> { Ok(Some("3c".repeat(32))) }
        fn previous(&self) -> Result<Option<String>, String> { Ok(None) }
    }

    /// 首次 fetch 阻塞在门闩上：先 notify（证明已进入下载），再等测试放行——
    /// 用于制造确定性的「下载中」窗口，验证并发 confirm 的 409。
    struct GatedChunks {
        sealed: Mutex<HashMap<String, Vec<u8>>>,
        gate: Mutex<Option<(mpsc::Sender<()>, mpsc::Receiver<()>)>>,
    }
    impl ChunkStore for GatedChunks {
        fn fetch(&self, token: &str) -> Result<Vec<u8>, String> {
            if let Some((notify, release)) = self.gate.lock().unwrap().take() {
                let _ = notify.send(());
                let _ = release.recv();
            }
            Ok(self.sealed.lock().unwrap().get(token).cloned().expect("未知分片"))
        }
        fn delete(&self, _token: &str) -> Result<(), String> { Ok(()) }
    }

    fn key() -> Vec<u8> {
        crypto::key_from_hex(&"3c".repeat(32)).expect("固定密钥")
    }

    /// 预占一个空闲端口（bind :0 后立刻释放；start 内重绑，极端情况靠重试兜底）
    fn free_port() -> u16 {
        let l = TcpListener::bind(("127.0.0.1", 0)).expect("bind :0");
        let port = l.local_addr().unwrap().port();
        drop(l);
        port
    }

    fn http_raw(port: u16, req: &str) -> (u16, String) {
        let mut s = TcpStream::connect(("127.0.0.1", port)).expect("连接本地服务");
        s.set_read_timeout(Some(Duration::from_secs(15))).expect("设读超时");
        s.write_all(req.as_bytes()).expect("发送请求");
        let mut buf = String::new();
        let _ = s.read_to_string(&mut buf);
        let status: u16 = buf
            .split_whitespace()
            .nth(1)
            .and_then(|s| s.parse().ok())
            .unwrap_or(0);
        (status, buf)
    }

    fn body_of(resp: &str) -> &str {
        resp.split("\r\n\r\n").nth(1).unwrap_or("")
    }

    fn handle_of(page_html: &str) -> String {
        let key = "handle=\"";
        let i = page_html.find(key).expect("页面必须含 handle 占位") + key.len();
        let rest = &page_html[i..];
        let j = rest.find('"').expect("handle 未闭合");
        rest[..j].to_string()
    }

    fn get_dl(port: u16, payload: &str, host: &str) -> (u16, String) {
        http_raw(
            port,
            &format!("GET /dl?t={payload} HTTP/1.1\r\nHost: {host}\r\nConnection: close\r\n\r\n"),
        )
    }

    fn post_confirm(port: u16, handle: &str, origin: Option<&str>, referer: Option<&str>, ctype: &str) -> (u16, String) {
        let body = format!("{{\"handle\":\"{handle}\"}}");
        let mut req = format!("POST /dl/confirm HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\n");
        if let Some(o) = origin {
            req.push_str(&format!("Origin: {o}\r\n"));
        }
        if let Some(r) = referer {
            req.push_str(&format!("Referer: {r}\r\n"));
        }
        req.push_str(&format!(
            "Content-Type: {ctype}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        ));
        http_raw(port, &req)
    }

    /// 一片信封：返回 (payload, 明文, token → 密文)
    fn make_payload() -> (String, Vec<u8>, HashMap<String, Vec<u8>>) {
        use base64::engine::general_purpose::URL_SAFE_NO_PAD as B64URL;
        use base64::Engine as _;
        let dek: Vec<u8> = (0u8..32).map(|i| 0x10u8.wrapping_add(i)).collect();
        let tid: Vec<u8> = (0u8..16).map(|i| 0x20u8.wrapping_add(i)).collect();
        let nonce: Vec<u8> = (0u8..12).map(|i| 0x30u8.wrapping_add(i)).collect();
        let plain: Vec<u8> = (0..32u32).map(|i| i as u8).collect();
        let sealed = crypto::seal_chunk(&dek, &nonce, &tid, 0, &plain).expect("加密分片");
        let env = Envelope {
            v: crypto::PROTO_VERSION,
            kid: crypto::fingerprint(&key()),
            dek: B64URL.encode(&dek),
            tid: crypto::hex(&tid),
            meta: Metadata {
                name: "u17.bin".into(),
                mime: String::new(),
                size: plain.len() as u64,
                sha256: crypto::hex(&Sha256::digest(&plain)),
                ts: crypto::now_unix(),
                chunks: vec![ChunkMeta {
                    t: "boxcnU17".into(),
                    n: 0,
                    off: 0,
                    size: plain.len() as u64,
                    nonce: B64URL.encode(&nonce),
                }],
            },
        };
        let payload = crypto::seal_payload(&key(), &env).expect("封装");
        (payload, plain, HashMap::from([("boxcnU17".to_string(), sealed)]))
    }

    #[test]
    fn u17_wire_contract_and_exit_gate() {
        let tmp = std::env::temp_dir().join(format!("qn-u17-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(&tmp).expect("建临时目录");

        let (payload, plain, sealed) = make_payload();
        let scripted = Arc::new(GatedChunks {
            sealed: Mutex::new(sealed),
            gate: Mutex::new(None),
        });
        let engine = Arc::new(
            Engine::open_with(
                tmp.join("work"),
                Arc::new(FixedHost::new(tmp.join("cfg"))
                    .with_download_dir(tmp.join("dl").to_string_lossy().to_string())),
                Arc::new(FixedKeys),
                Some(scripted.clone() as Arc<dyn ChunkStore>),
            )
            .expect("构建 Engine"),
        );
        let cell = Arc::new(StatusCell::new(LocalServiceStatus::Stopped));
        let svc = RealLocalService::new(cell, engine);

        let mut port = None;
        for _ in 0..10 {
            let p = free_port();
            if let LocalServiceStatus::Running { bound_port } = svc.start(p, TEST_PAGE) {
                port = Some(bound_port);
                break;
            }
        }
        let port = port.expect("拿到可用端口");
        let host = format!("127.0.0.1:{port}");
        let expect = format!("http://127.0.0.1:{port}");

        /* Host 校验：非 127.0.0.1:<port> → 403（防 DNS rebinding） */
        let (st, resp) = get_dl(port, &payload, "evil.example:1");
        assert_eq!(st, 403, "{resp}");
        assert!(resp.contains("Host 不合法"));

        /* 未知路由 → 404 */
        let (st, _) = http_raw(port, &format!("GET /nope HTTP/1.1\r\nHost: {host}\r\nConnection: close\r\n\r\n"));
        assert_eq!(st, 404);

        /* 缺载荷 → 410 */
        let (st, _) = get_dl(port, "", &host);
        assert_eq!(st, 410);

        /* 正常 GET ×3：同一指纹三个一次性 handle */
        let (st, r1) = get_dl(port, &payload, &host);
        assert_eq!(st, 200, "{r1}");
        assert!(r1.contains("data-state=\"pending\""), "{r1}");
        let h1 = handle_of(&r1);
        let (_, r2) = get_dl(port, &payload, &host);
        let h2 = handle_of(&r2);
        let (_, r3) = get_dl(port, &payload, &host);
        let h3 = handle_of(&r3);
        assert_ne!(h1, h2);
        assert_ne!(h2, h3);

        /* POST 校验顺序 ①：Origin/Referer 缺或跨 → 403，且不消耗 handle */
        let (st, resp) = post_confirm(port, &h1, Some("http://evil.example"), None, "application/json");
        assert_eq!(st, 403, "{resp}");
        let (st, _) = post_confirm(port, &h1, None, Some("http://127.0.0.1:1/dl"), "application/json");
        assert_eq!(st, 403);
        let (st, _) = post_confirm(port, &h1, None, None, "application/json");
        assert_eq!(st, 403, "无 Origin 且无 Referer 必须拒绝");

        /* POST 校验顺序 ②：Content-Type 必须 application/json */
        let (st, resp) = post_confirm(port, &h1, Some(&expect), None, "text/plain");
        assert_eq!(st, 400, "{resp}");
        assert!(resp.contains("application/json"));

        /* 正确 confirm → 进入下载（阻塞在门闩） */
        let (tx_notify, rx_notify) = mpsc::channel();
        let (tx_release, rx_release) = mpsc::channel();
        *scripted.gate.lock().unwrap() = Some((tx_notify, rx_release));
        let expect_c = expect.clone();
        let worker = std::thread::spawn(move || post_confirm(port, &h1, Some(&expect_c), None, "application/json"));
        rx_notify
            .recv_timeout(Duration::from_secs(5))
            .expect("confirm 应已进入第 1 片下载");

        /* 重复 handle 幂等（进行中）：同指纹第二个 handle → 409 */
        let (st, resp) = post_confirm(port, &h2, Some(&expect), None, "application/json");
        assert_eq!(st, 409, "{resp}");
        assert!(resp.contains("正在下载"));

        /* 放行 → 下载完成 → 200 + final_path */
        let _ = tx_release.send(());
        let (st, resp) = worker.join().expect("confirm 线程不应 panic");
        assert_eq!(st, 200, "{resp}");
        let v: serde_json::Value = serde_json::from_str(body_of(&resp)).expect("JSON 响应");
        assert_eq!(v["code"], 0);
        let final_path = v["final_path"].as_str().expect("应有 final_path");
        assert_eq!(std::fs::read(final_path).expect("读回"), plain, "落盘内容必须一致");

        /* 重复 handle 幂等（已完成）：第三个 handle → 200 replay */
        let (st, resp) = post_confirm(port, &h3, Some(&expect), None, "application/json");
        assert_eq!(st, 200, "{resp}");
        let v: serde_json::Value = serde_json::from_str(body_of(&resp)).expect("JSON 响应");
        assert_eq!(v["replay"], true, "已完成指纹必须幂等回放: {v}");

        /* 退出门闩（对齐 A19）：stop_accepting 后新请求一律 503；恢复后正常 */
        svc.stop_accepting();
        let (st, resp) = get_dl(port, &payload, &host);
        assert_eq!(st, 503, "{resp}");
        assert!(resp.contains("服务停止中"));
        svc.resume_accepting();
        let (st, resp) = get_dl(port, &payload, &host);
        assert_eq!(st, 200, "恢复准入后应正常响应: {resp}");
        assert!(resp.contains("data-state=\"consumed\""), "已消费指纹应渲染「已下载过」页: {resp}");

        /* 清理：join 全部 worker */
        svc.stop();
        let _ = std::fs::remove_dir_all(&tmp);
    }
}
