//! M0a 出口 3 剩余三条纯本地用例：U7（ts 过期）/ U13（断点续传）/ U14（下载目录）。
//!
//! 全部经注入的 `KeySource` / `ChunkStore` 运行——不触碰真实凭据库（真实主密钥
//! 就在 keyring 条目下，绝不能动）、不碰真实租户、不发网络请求。

use base64::engine::general_purpose::URL_SAFE_NO_PAD as B64URL;
use base64::Engine as _;
use qingniao_core::transfer::crypto::{self, ChunkMeta, Envelope, Metadata};
use qingniao_core::transfer::engine::{ChunkStore, ClaimResult, Engine, FixedHost, Host, KeySource};
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

/* ===================== 固定向量（与 transfer_tamper 同源） ===================== */

fn key() -> Vec<u8> {
    crypto::key_from_hex(&"3c".repeat(32)).expect("固定密钥")
}
fn dek() -> Vec<u8> {
    (0u8..32).map(|i| 0x10u8.wrapping_add(i)).collect()
}
fn tid() -> Vec<u8> {
    (0u8..16).map(|i| 0x20u8.wrapping_add(i)).collect()
}

/// 固定密钥来源（U7：不经 keyring）
struct FixedKeys;
impl KeySource for FixedKeys {
    fn current(&self) -> Result<Option<String>, String> { Ok(Some(crypto::hex(&key()))) }
    fn previous(&self) -> Result<Option<String>, String> { Ok(None) }
}

/// 无密钥来源（「尚未配置」分支）
struct NoKeys;
impl KeySource for NoKeys {
    fn current(&self) -> Result<Option<String>, String> { Ok(None) }
    fn previous(&self) -> Result<Option<String>, String> { Ok(None) }
}

/// 构造信封：按真实分片规则切片（非末片 = CHUNK_SIZE，与 `validate_metadata` 的
/// 自洽约束一致）；返回 (信封, [(token, 密文)])
fn make_env(plain: &[u8], ts: i64) -> (Envelope, Vec<(String, Vec<u8>)>) {
    let mut metas = Vec::new();
    let mut sealed_pairs = Vec::new();
    for (i, slice) in plain.chunks(crypto::CHUNK_SIZE).enumerate() {
        let nonce: Vec<u8> = (0u8..12).map(|j| 0x30u8.wrapping_add(j + i as u8)).collect();
        let sealed = crypto::seal_chunk(&dek(), &nonce, &tid(), i as u32, slice).expect("加密");
        let token = format!("boxcnU{i}");
        metas.push(ChunkMeta {
            t: token.clone(),
            n: i as u32,
            off: (i * crypto::CHUNK_SIZE) as u64,
            size: slice.len() as u64,
            nonce: B64URL.encode(&nonce),
        });
        sealed_pairs.push((token, sealed));
    }
    let env = Envelope {
        v: crypto::PROTO_VERSION,
        kid: crypto::fingerprint(&key()),
        dek: B64URL.encode(dek()),
        tid: crypto::hex(&tid()),
        meta: Metadata {
            name: "u-case.bin".into(),
            mime: String::new(),
            size: plain.len() as u64,
            sha256: crypto::hex(&Sha256::digest(plain)),
            ts,
            chunks: metas,
        },
    };
    (env, sealed_pairs)
}

/* ===================== 脚本化分片存取（U13 的可控失败点） ===================== */

struct Scripted {
    sealed: Mutex<HashMap<String, Vec<u8>>>,
    /// fetch 调用顺序（token）
    fetch_calls: Mutex<Vec<String>>,
    /// token → 前几次调用返回 Err（跨 Engine 实例累计计数）
    fail_first: Mutex<HashMap<String, usize>>,
    /// token → 累计调用次数
    calls: Mutex<HashMap<String, usize>>,
}

impl ChunkStore for Scripted {
    fn fetch(&self, token: &str) -> Result<Vec<u8>, String> {
        self.fetch_calls.lock().unwrap().push(token.to_string());
        {
            let mut calls = self.calls.lock().unwrap();
            let e = calls.entry(token.to_string()).or_insert(0);
            let k = *e;
            *e += 1;
            if let Some(&n_fail) = self.fail_first.lock().unwrap().get(token) {
                if k < n_fail {
                    return Err(format!("注入失败 #{k}"));
                }
            }
        }
        Ok(self
            .sealed
            .lock()
            .unwrap()
            .get(token)
            .cloned()
            .expect("未知分片 token"))
    }
    fn delete(&self, _token: &str) -> Result<(), String> { Ok(()) }
}

// （calls 字段在 fetch 中按 token 计数，与 fetch_calls 互为佐证）

static TMP_SEQ: AtomicU64 = AtomicU64::new(0);

fn temp_root(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "qn-u-case-{}-{}-{}",
        tag,
        std::process::id(),
        TMP_SEQ.fetch_add(1, Ordering::Relaxed)
    ));
    std::fs::create_dir_all(&dir).expect("建临时目录");
    dir
}

fn engine_at(root: &PathBuf, download_dir: &str, store: Option<Arc<dyn ChunkStore>>) -> Engine {
    Engine::open_with(
        root.join("work"),
        Arc::new(FixedHost::new(root.join("cfg")).with_download_dir(download_dir)),
        Arc::new(FixedKeys),
        store,
    )
    .expect("构建 Engine")
}

/* ===================== U7：重放旧链接（>10 min）→ ts 过期拒绝（D8） ===================== */

#[test]
fn u7_expired_payload_is_rejected() {
    let root = temp_root("u7-expired");
    let engine = engine_at(&root, root.join("dl").to_str().unwrap(), None);

    let old_ts = crypto::now_unix() - crypto::FRESHNESS_WINDOW_SECS - 5;
    let (env, _) = make_env(b"u7-expired-payload", old_ts);
    let payload = crypto::seal_payload(&key(), &env).expect("封装");

    let err = engine.evaluate_payload(&payload).unwrap_err();
    assert!(err.contains("链接已过期"), "实际错误：{err}");
    assert!(err.contains(&crypto::FRESHNESS_WINDOW_MINUTES.to_string()), "错误应含窗口分钟数：{err}");
}

#[test]
fn u7_fresh_payload_passes_with_injected_key() {
    let root = temp_root("u7-fresh");
    let engine = engine_at(&root, root.join("dl").to_str().unwrap(), None);

    let (env, _) = make_env(b"u7-fresh-payload", crypto::now_unix());
    let payload = crypto::seal_payload(&key(), &env).expect("封装");

    let ev = engine.evaluate_payload(&payload).expect("新鲜 payload 必须通过");
    assert_eq!(ev.key_hex, crypto::hex(&key()), "应选中注入的密钥");
    assert_eq!(ev.fingerprint, crypto::payload_fingerprint(&payload));
}

#[test]
fn u7_no_key_reports_configuration_error() {
    let root = temp_root("u7-nokey");
    // 显式注入「无密钥」来源，覆盖「尚未配置」分支而不碰真实凭据库
    let engine = Engine::open_with(
        root.join("work"),
        Arc::new(FixedHost::new(root.join("cfg"))),
        Arc::new(NoKeys),
        None,
    )
    .expect("构建 Engine");

    let (env, _) = make_env(b"u7-no-key", crypto::now_unix());
    let payload = crypto::seal_payload(&key(), &env).expect("封装");

    let err = engine.evaluate_payload(&payload).unwrap_err();
    assert!(err.contains("尚未配置传输密钥"), "实际错误：{err}");
}

/* ===================== U13：断点续传（下载中断重试，已完成片跳过） ===================== */

#[test]
fn u13_resume_after_midway_failure_skips_completed_chunks() {
    let root = temp_root("u13");
    let dl = root.join("dl");
    // 3 片 = 3 × CHUNK_SIZE（validate_metadata 要求非末片必须整片）
    let plain: Vec<u8> = (0..3 * crypto::CHUNK_SIZE).map(|i| (i % 251) as u8).collect();
    let (env, pairs) = make_env(&plain, crypto::now_unix());
    let payload = crypto::seal_payload(&key(), &env).expect("封装");
    let tokens: Vec<String> = pairs.iter().map(|(t, _)| t.clone()).collect();

    let scripted = Arc::new(Scripted {
        sealed: Mutex::new(pairs.into_iter().collect()),
        fetch_calls: Mutex::new(Vec::new()),
        fail_first: Mutex::new(tokens[1..2].iter().map(|t| (t.clone(), 1)).collect()),
        calls: Mutex::new(HashMap::new()),
    });
    let engine = engine_at(&root, dl.to_str().unwrap(), Some(scripted.clone()));

    /* 第一次：在第 2 片注入失败 */
    let ev = engine.evaluate_payload(&payload).expect("校验");
    let view = engine.create_session(&payload, ev).expect("建会话");
    let pending = match engine.claim_session(&view.handle).expect("领取") {
        ClaimResult::Start(p) => p,
        ClaimResult::AlreadyDone { .. } => panic!("首次不应已完成"),
    };
    let err = engine.run_download_sync(&pending).unwrap_err();
    assert!(err.contains("注入失败"), "实际错误：{err}");

    // §9.4：失败（非取消）保留 .part 与侧车以便续传
    let sidecar_path = dl
        .read_dir()
        .expect("读下载目录")
        .map(|e| e.unwrap().path())
        .find(|p| p.extension().map(|x| x == "json").unwrap_or(false))
        .expect("失败后必须保留侧车文件");
    let sidecar: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(&sidecar_path).unwrap()).unwrap();
    assert_eq!(sidecar["completed"], serde_json::json!([0]), "仅第 1 片应已记录");
    assert!(dl.read_dir().unwrap().any(|e| e.unwrap().path().extension().map(|x| x == "part").unwrap_or(false)),
        "失败后必须保留 .part");

    /* 第二次：新会话重试——已完成片必须跳过 */
    let ev2 = engine.evaluate_payload(&payload).expect("校验");
    let view2 = engine.create_session(&payload, ev2).expect("建会话");
    let pending2 = match engine.claim_session(&view2.handle).expect("领取") {
        ClaimResult::Start(p) => p,
        ClaimResult::AlreadyDone { .. } => panic!("未成功过，不应命中幂等"),
    };
    let final_path = engine.run_download_sync(&pending2).expect("重试必须成功");

    assert_eq!(std::fs::read(&final_path).expect("读回"), plain, "合并结果必须与原文一致");

    // 片级断言：第 1 片全程只 fetch 过一次（重试时跳过）；第 2 片两次；第 3 片一次
    let calls = scripted.fetch_calls.lock().unwrap();
    let count = |t: &str| calls.iter().filter(|c| c.as_str() == t).count();
    assert_eq!(count(&tokens[0]), 1, "已完成片在重试时必须被跳过");
    assert_eq!(count(&tokens[1]), 2, "失败片应被重新拉取");
    assert_eq!(count(&tokens[2]), 1);

    // 收尾：.part / 侧车已消失（rename + 删除），云端分片删除全部成功
    assert!(!dl.read_dir().unwrap().any(|e| {
        let p = e.unwrap().path();
        p.extension().map(|x| x == "part" || x == "json").unwrap_or(false)
    }));
}

/* ===================== U14：固定目录配置 ===================== */

#[test]
fn u14_custom_dir_is_created_and_used_by_session() {
    let root = temp_root("u14-custom");
    // 两层不存在的目录：验证「目录不存在自动创建」
    let custom = root.join("my-downloads").join("nested");

    let engine = engine_at(&root, custom.to_str().unwrap(), None);
    let (env, _) = make_env(b"u14", crypto::now_unix());
    let payload = crypto::seal_payload(&key(), &env).expect("封装");

    let ev = engine.evaluate_payload(&payload).expect("校验");
    let view = engine.create_session(&payload, ev).expect("建会话");
    assert_eq!(view.download_dir, custom.to_string_lossy().to_string());
    assert!(custom.is_dir(), "目录不存在时必须自动创建");
}

#[test]
fn u14_default_falls_back_to_home_downloads() {
    let root = temp_root("u14-default");
    let host = FixedHost::new(root.join("cfg"));
    let home = std::env::var("HOME").expect("测试环境必须有 HOME");
    assert_eq!(
        host.resolve_download_dir().expect("解析"),
        PathBuf::from(&home).join("Downloads").to_string_lossy().to_string()
    );
}

#[test]
fn u14_empty_config_string_also_falls_back() {
    let root = temp_root("u14-empty");
    let host = FixedHost::new(root.join("cfg")).with_download_dir("");
    let home = std::env::var("HOME").expect("测试环境必须有 HOME");
    assert_eq!(
        host.resolve_download_dir().expect("解析"),
        PathBuf::from(&home).join("Downloads").to_string_lossy().to_string()
    );
}
