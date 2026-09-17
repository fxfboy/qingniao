//! M3 集成测试：APP 侧 send_message / resend_payload / analyze 与 core 的对接
//! （mock webhook 同 CLI 集成测试；方案 v3 §九 DoD：发送回归 + golden 一致）

use qingniao_lib::{analyze_impl, resend_payload_impl, send_message_impl};
use serde_json::{json, Value};
use std::io::{Read, Write};
use std::net::TcpListener;
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

fn temp_dir(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "qn-app-test-{}-{}-{}",
        tag,
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn spawn_mock(response_body: &'static str) -> (String, Arc<Mutex<String>>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let last_body = Arc::new(Mutex::new(String::new()));
    let last2 = last_body.clone();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { continue };
            let mut data = Vec::new();
            let mut buf = [0u8; 65536];
            loop {
                match stream.read(&mut buf) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => {
                        data.extend_from_slice(&buf[..n]);
                        if let Some(pos) = data.windows(4).position(|w| w == b"\r\n\r\n") {
                            let head = String::from_utf8_lossy(&data[..pos]).to_lowercase();
                            let cl: usize = head
                                .lines()
                                .find_map(|l| l.strip_prefix("content-length:"))
                                .and_then(|v| v.trim().parse().ok())
                                .unwrap_or(0);
                            if data.len() >= pos + 4 + cl {
                                break;
                            }
                        }
                    }
                }
            }
            if let Some(pos) = data.windows(4).position(|w| w == b"\r\n\r\n") {
                *last2.lock().unwrap() = String::from_utf8_lossy(&data[pos + 4..]).to_string();
            }
            let resp = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                response_body.len(),
                response_body
            );
            let _ = stream.write_all(resp.as_bytes());
            let _ = stream.flush();
        }
    });
    (format!("http://{}", addr), last_body)
}

fn write_config(path: &std::path::Path, doc: Value) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, serde_json::to_string_pretty(&doc).unwrap()).unwrap();
}

fn read_config(path: &std::path::Path) -> Value {
    serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap()
}

#[test]
fn send_message_writes_history_with_app_extra() {
    let (url, last_body) = spawn_mock(r#"{"code":0,"msg":"success"}"#);
    let dir = temp_dir("app-send");
    let cfg_path = dir.join("qingniao.json");
    write_config(
        &cfg_path,
        json!({
            "webhooks": [{ "name": "mock", "url": url, "secret": "sec" }],
            "history": [], "last_webhook": 0, "last_type": "auto",
            "APP_ONLY": 1,
        }),
    );

    let out = send_message_impl(
        &cfg_path,
        None,
        "post".into(),
        "# 标题\n正文".into(),
        vec![],
        None,
        None,
        Some(json!({ "text": "# 标题\n正文", "media": { "thumbs": [] } })),
        1700000000,
    )
    .unwrap();
    assert!(out.ok);
    assert_eq!(out.state, "sent");
    assert_eq!(out.status_line, "200 OK");

    let body: Value = serde_json::from_str(last_body.lock().unwrap().trim()).unwrap();
    assert_eq!(body["msg_type"], "post");
    assert!(body["sign"].is_string(), "APP 发送同样必须带签名");

    let doc = read_config(&cfg_path);
    assert_eq!(doc["history"].as_array().unwrap().len(), 1);
    let rec = &doc["history"][0];
    assert_eq!(rec["kind"], "post");
    assert_eq!(rec["state"], "sent");
    assert_eq!(rec["bot"], "mock");
    assert_eq!(rec["text"], "# 标题\n正文");
    assert!(rec["media"].is_object());
    assert!(rec["payload"].get("sign").is_none());
    assert_eq!(doc["APP_ONLY"], 1, "APP 专属字段不得被清除");
}

#[test]
fn send_message_auto_detect_and_business_fail() {
    let (url, _) = spawn_mock(r#"{"code":19021,"msg":"sign match error"}"#);
    let dir = temp_dir("app-auto");
    let cfg_path = dir.join("qingniao.json");
    write_config(
        &cfg_path,
        json!({ "webhooks": [{ "name": "m", "url": url, "secret": "" }], "history": [] }),
    );

    // auto：**加粗** → post
    let out = send_message_impl(&cfg_path, None, "auto".into(), "**粗**".into(), vec![], None, None, None, 0)
        .unwrap();
    assert!(!out.ok, "飞书业务错误应为失败");
    assert_eq!(out.state, "failed");
    assert_eq!(out.status_line, "19021 sign match error");
    assert_eq!(out.error.as_deref(), None, "HTTP 层完成不算 impl 级错误");
    let doc = read_config(&cfg_path);
    assert_eq!(doc["history"][0]["kind"], "post", "auto 识别应在 core 内完成");
}

#[test]
fn resend_updates_existing_record() {
    let (url, _) = spawn_mock(r#"{"code":0,"msg":"success"}"#);
    let dir = temp_dir("app-resend");
    let cfg_path = dir.join("qingniao.json");
    let rec_time = "2026-09-17T10:00:00.000Z";
    write_config(
        &cfg_path,
        json!({
            "webhooks": [{ "name": "m", "url": url, "secret": "" }],
            "history": [{ "time": rec_time, "kind": "text", "dir": "out", "summary": "hi",
                          "ok": false, "status": "网络失败", "state": "failed",
                          "payload": { "msg_type": "text", "content": { "text": "hi" } } }],
        }),
    );

    let out = resend_payload_impl(
        &cfg_path,
        json!({ "msg_type": "text", "content": { "text": "hi" } }),
        None,
        rec_time.into(),
        1700000001,
    )
    .unwrap();
    assert!(out.ok);
    assert_eq!(out.state, "sent");

    let doc = read_config(&cfg_path);
    let history = doc["history"].as_array().unwrap();
    assert_eq!(history.len(), 1, "重发更新原记录，不追加");
    assert_eq!(history[0]["ok"], true);
    assert_eq!(history[0]["status"], "200 OK");
    assert_eq!(history[0]["state"], "sent");
}

#[test]
fn analyze_matches_js_baseline() {
    // 与 golden 同源的识别断言（spot check）
    let d = analyze_impl("**加粗** 重点", 0);
    assert_eq!(d["t"], "post");
    let d = analyze_impl("{\"config\": {}}", 0);
    assert_eq!(d["t"], "interactive");
    let d = analyze_impl("", 2);
    assert_eq!(d["t"], "image");
    let d = analyze_impl("纯文字", 0);
    assert_eq!(d["t"], "text");
}
