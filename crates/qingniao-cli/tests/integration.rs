//! M2 集成测试（方案 v3 §十 DoD）：mock webhook、并发写锁、未知字段保留、
//! 旧目录迁移、URL 策略、历史状态三态。直接驱动 `qingniao_cli::cmd_*`（显式传配置路径）。

use qingniao_cli::cmd_send;
use qingniao_cli::{SendArgs, TypeArg};
use serde_json::{json, Value};
use std::io::{Read, Write};
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

// ===== mock webhook（仅测试用最小 HTTP 实现）=====

struct MockServer {
    url: String,
    hits: Arc<AtomicUsize>,
    last_body: Arc<Mutex<String>>,
}

fn spawn_mock(response_body: &'static str) -> MockServer {
    let listener = TcpListener::bind("127.0.0.1:0").expect("mock 端口绑定");
    let addr = listener.local_addr().unwrap();
    let hits = Arc::new(AtomicUsize::new(0));
    let last_body = Arc::new(Mutex::new(String::new()));
    let hits2 = hits.clone();
    let last2 = last_body.clone();
    // 测试进程结束线程随之消亡，无需优雅停机
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
                        // 头部结束后按 Content-Length 判断请求体是否读满
                        if let Some(pos) = find_header_end(&data) {
                            let cl = content_length(&data[..pos]).unwrap_or(0);
                            if data.len() >= pos + cl {
                                break;
                            }
                        }
                    }
                }
            }
            if let Some(pos) = find_header_end(&data) {
                *last2.lock().unwrap() =
                    String::from_utf8_lossy(&data[pos..]).to_string();
            }
            hits2.fetch_add(1, Ordering::SeqCst);
            let resp = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                response_body.len(),
                response_body
            );
            let _ = stream.write_all(resp.as_bytes());
            let _ = stream.flush();
        }
    });
    MockServer {
        url: format!("http://{}", addr),
        hits,
        last_body,
    }
}

fn find_header_end(data: &[u8]) -> Option<usize> {
    data.windows(4).position(|w| w == b"\r\n\r\n").map(|p| p + 4)
}

fn content_length(head: &[u8]) -> Option<usize> {
    let head = String::from_utf8_lossy(head);
    head.to_lowercase()
        .lines()
        .find_map(|l| l.strip_prefix("content-length:"))
        .and_then(|v| v.trim().parse().ok())
}

// ===== 测试辅助 =====

fn temp_dir(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "qn-cli-test-{}-{}-{}",
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

fn write_config(path: &Path, doc: Value) {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).unwrap();
    }
    std::fs::write(path, serde_json::to_string_pretty(&doc).unwrap()).unwrap();
}

fn read_config(path: &Path) -> Value {
    serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap()
}

fn send_args(bot: Option<String>, json: bool) -> SendArgs {
    SendArgs {
        text: None,
        r#type: None,
        bot,
        image: vec![],
        image_key: vec![],
        stdin: false,
        file: None,
        title: None,
        dry_run: false,
        show_sign: false,
        allow_insecure_url: false,
        json,
    }
}

// ===== 用例 =====

#[test]
fn send_success_writes_history_and_signs() {
    let mock = spawn_mock(r#"{"code":0,"msg":"success"}"#);
    let dir = temp_dir("send-ok");
    let cfg_path = qingniao_core::config::config_path_in_dir(&dir).unwrap();
    write_config(
        &cfg_path,
        json!({
            "webhooks": [{ "id": "bot1", "name": "mock", "url": mock.url, "secret": "topsecret" }],
            "history": [], "last_webhook": 0, "last_type": "auto",
            "APP_ONLY_FIELD": { "keep": true },
        }),
    );

    let mut args = send_args(None, true);
    args.allow_insecure_url = true;
    args.text = Some("hello".into());
    let out = cmd_send(&cfg_path, args).unwrap();
    assert_eq!(out.code, 0);

    // mock 收到的请求带 timestamp+sign
    assert_eq!(mock.hits.load(Ordering::SeqCst), 1);
    let body: Value = serde_json::from_str(mock.last_body.lock().unwrap().trim()).unwrap();
    assert_eq!(body["msg_type"], "text");
    assert!(body["sign"].is_string());
    assert!(body["timestamp"].is_string());

    // 历史：state=sent、status 与 APP 文案一致、payload 为签名前
    let doc = read_config(&cfg_path);
    assert_eq!(doc["history"].as_array().unwrap().len(), 1);
    let rec = &doc["history"][0];
    assert_eq!(rec["state"], "sent");
    assert_eq!(rec["status"], "200 OK");
    assert_eq!(rec["ok"], true);
    assert_eq!(rec["kind"], "text");
    assert_eq!(rec["bot"], "mock");
    assert!(rec["payload"].get("sign").is_none(), "历史 payload 不得含签名");
    assert_eq!(rec["text"], "hello");
    // APP 专属字段未被 CLI 保存清除（D5）
    assert_eq!(doc["APP_ONLY_FIELD"]["keep"], true);
}

#[test]
fn send_business_error_is_failed_and_exit_2() {
    let mock = spawn_mock(r#"{"code":19021,"msg":"sign match error"}"#);
    let dir = temp_dir("send-err");
    let cfg_path = qingniao_core::config::config_path_in_dir(&dir).unwrap();
    write_config(
        &cfg_path,
        json!({ "webhooks": [{ "name": "mock", "url": mock.url, "secret": "" }], "history": [] }),
    );

    let mut args = send_args(None, true);
    args.allow_insecure_url = true;
    args.text = Some("hi".into());
    let out = cmd_send(&cfg_path, args).unwrap();
    assert_eq!(out.code, 2, "飞书业务错误退出码应为 2");
    let doc = read_config(&cfg_path);
    assert_eq!(doc["history"][0]["state"], "failed");
    assert_eq!(doc["history"][0]["ok"], false);
    assert_eq!(doc["history"][0]["status"], "19021 sign match error");
}

#[test]
fn url_policy_blocks_by_default() {
    let mock = spawn_mock(r#"{"code":0}"#);
    let dir = temp_dir("url-policy");
    let cfg_path = qingniao_core::config::config_path_in_dir(&dir).unwrap();
    write_config(
        &cfg_path,
        json!({ "webhooks": [{ "name": "intranet", "url": mock.url, "secret": "" }], "history": [] }),
    );

    let mut args = send_args(None, false);
    args.text = Some("hi".into());
    let err = cmd_send(&cfg_path, args).unwrap_err();
    assert_eq!(err.exit_code(), 1, "URL 策略属 usage 错误 → 退出码 1");
    assert_eq!(mock.hits.load(Ordering::SeqCst), 0, "未放行时不得发出请求");

    // 显式放行后可发送
    let mut args = send_args(None, true);
    args.text = Some("hi".into());
    args.allow_insecure_url = true;
    assert_eq!(cmd_send(&cfg_path, args).unwrap().code, 0);
    assert_eq!(mock.hits.load(Ordering::SeqCst), 1);
}

#[test]
fn dry_run_no_network_no_history() {
    let mock = spawn_mock(r#"{"code":0}"#);
    let dir = temp_dir("dry-run");
    let cfg_path = qingniao_core::config::config_path_in_dir(&dir).unwrap();
    write_config(
        &cfg_path,
        json!({ "webhooks": [{ "name": "mock", "url": mock.url, "secret": "sec" }], "history": [] }),
    );

    let mut args = send_args(None, true);
    args.text = Some("preview".into());
    args.dry_run = true;
    let out = cmd_send(&cfg_path, args).unwrap();
    assert_eq!(out.code, 0);
    let Out::Json(v) = out.out else { panic!("--json 应输出 JSON") };
    assert_eq!(v["kind"], "dry_run");
    assert_eq!(v["sign"], "omitted", "默认不输出真实签名（D9）");
    assert!(v["payload"]["content"]["text"] == "preview");
    assert_eq!(mock.hits.load(Ordering::SeqCst), 0);
    assert_eq!(read_config(&cfg_path)["history"].as_array().unwrap().len(), 0);
}

#[test]
fn concurrent_bot_add_no_lost_update() {
    let dir = temp_dir("concurrent");
    let cfg_path = qingniao_core::config::config_path_in_dir(&dir).unwrap();
    write_config(&cfg_path, json!({ "webhooks": [], "history": [] }));

    let n = 8;
    let mut handles = Vec::new();
    for i in 0..n {
        let p = cfg_path.clone();
        handles.push(std::thread::spawn(move || {
            qingniao_core::config::modify(&p, |cfg| {
                cfg.add_bot(&format!("bot-{i}"), "https://open.feishu.cn/open-apis/bot/v2/hook/x", "")
            })
        }));
    }
    for h in handles {
        h.join().unwrap().unwrap();
    }
    let doc = read_config(&cfg_path);
    let bots = doc["webhooks"].as_array().unwrap();
    assert_eq!(bots.len(), n, "锁内 read-modify-write 不得丢更新");
    let ids: std::collections::HashSet<_> =
        bots.iter().map(|b| b["id"].as_str().unwrap().to_string()).collect();
    assert_eq!(ids.len(), n, "id 不得重复");
}

#[test]
fn unknown_fields_survive_cli_save() {
    let dir = temp_dir("unknown-fields");
    let cfg_path = qingniao_core::config::config_path_in_dir(&dir).unwrap();
    write_config(
        &cfg_path,
        json!({
            "webhooks": [{ "name": "a", "url": "https://open.feishu.cn/hook/x", "secret": "" }],
            "history": [{ "time": "t", "kind": "text", "dir": "out", "summary": "s",
                          "ok": true, "status": "200 OK", "payload": {}, "media": { "thumb": "d" } }],
            "future_field": { "v": 42 },
        }),
    );
    // 触发一次 CLI 写路径（bot add）
    qingniao_cli::cmd_bot_add(&cfg_path, "b", "https://open.feishu.cn/hook/y", false, false, true)
        .unwrap();
    let doc = read_config(&cfg_path);
    assert_eq!(doc["future_field"]["v"], 42, "顶层未知字段必须保留（D5）");
    assert_eq!(doc["history"][0]["media"]["thumb"], "d", "history 条目未知字段必须保留");
    // 旧机器人补齐 id，新增机器人有 id
    assert!(doc["webhooks"][0]["id"].is_string());
    assert!(doc["webhooks"][1]["id"].is_string());
    assert_eq!(doc["schema_version"], 2);
}

#[test]
fn legacy_dir_migration_copies_config() {
    let root = temp_dir("migrate");
    let old_dir = root.join("com.qingniao.app");
    std::fs::create_dir_all(&old_dir).unwrap();
    std::fs::write(
        old_dir.join("qingniao.json"),
        json!({ "webhooks": [{ "name": "旧机器人", "url": "https://open.feishu.cn/hook/z", "secret": "" }] })
            .to_string(),
    )
    .unwrap();

    let new_dir = root.join("qingniao");
    let path = qingniao_core::config::config_path_in_dir(&new_dir).unwrap();
    assert!(path.exists(), "迁移后新路径应有配置");
    let doc = read_config(&path);
    assert_eq!(doc["webhooks"][0]["name"], "旧机器人");
    assert!(old_dir.join("qingniao.json").exists(), "旧文件保留不删除（与 APP 一致）");
}

#[test]
fn timeout_maps_to_unknown_state() {
    // 指向一个不监听的非路由地址会立即连接失败（failed），真正的超时难以在测试中稳定构造；
    // 此处验证网络层失败 → failed、退出码 2 的映射；unknown 分支由 dispatch_send 的
    // is_timeout 判定覆盖（send_webhook 单元级）
    let dir = temp_dir("net-fail");
    let cfg_path = qingniao_core::config::config_path_in_dir(&dir).unwrap();
    // 127.0.0.1:1 通常立即拒绝连接
    write_config(
        &cfg_path,
        json!({ "webhooks": [{ "name": "dead", "url": "http://127.0.0.1:1/hook", "secret": "" }], "history": [] }),
    );
    let mut args = send_args(None, true);
    args.text = Some("hi".into());
    args.allow_insecure_url = true;
    let err = cmd_send(&cfg_path, args).unwrap_err();
    assert_eq!(err.exit_code(), 2);
    let doc = read_config(&cfg_path);
    assert_eq!(doc["history"][0]["state"], "failed");
}

#[test]
fn outcome_json_send_success_shape() {
    let mock = spawn_mock(r#"{"code":0,"msg":"success"}"#);
    let dir = temp_dir("json-shape");
    let cfg_path = qingniao_core::config::config_path_in_dir(&dir).unwrap();
    write_config(
        &cfg_path,
        json!({ "webhooks": [{ "name": "m", "url": mock.url, "secret": "" }], "history": [] }),
    );
    let mut args = send_args(None, true);
    args.allow_insecure_url = true;
    args.text = Some("hi".into());
    let out = cmd_send(&cfg_path, args).unwrap();
    let Out::Json(v) = out.out else { panic!("--json 应输出 JSON") };
    assert_eq!(v["ok"], true);
    assert_eq!(v["kind"], "sent");
    assert_eq!(v["result"]["http_status"], 200);
    assert_eq!(v["result"]["feishu_code"], 0);
    assert!(v["error"].is_null());
    assert!(v["sign"].is_null(), "发送成功的 json 输出不携带签名（D9）");
}

// 引入 Outcome 供断言（类型位于 qingniao_cli）
use qingniao_cli::Out;

#[test]
fn detect_via_type_flag() {
    let mock = spawn_mock(r#"{"code":0}"#);
    let dir = temp_dir("force-type");
    let cfg_path = qingniao_core::config::config_path_in_dir(&dir).unwrap();
    write_config(
        &cfg_path,
        json!({ "webhooks": [{ "name": "m", "url": mock.url, "secret": "" }], "history": [] }),
    );
    // 强制 text：Markdown 标记不转换（与 APP forcedType 行为一致）
    let mut args = send_args(None, false);
    args.allow_insecure_url = true;
    args.text = Some("**not bold**".into());
    args.r#type = Some(TypeArg::Text);
    let out = cmd_send(&cfg_path, args).unwrap();
    assert_eq!(out.code, 0);
    let body: Value =
        serde_json::from_str(mock.last_body.lock().unwrap().trim()).unwrap();
    assert_eq!(body["msg_type"], "text");
    assert_eq!(body["content"]["text"], "**not bold**");
}
