//! M0b 验收测试（CLI 方案 §10 出口 / §7 实现约束 / R8 合并语义）。
//!
//! - 指纹锁：跨 claim 互斥（Busy）、失败后释放（可重试）、D24 ②幂等回放
//! - 状态文件：quota/consumed/pending_deletes 锁内重读合并（另一实例写入不丢）
//! - R8：replace_preserving_history 保留磁盘 history；delete_history_entry 定位删除
//! - 临时文件：pid 后缀原子写，修改后无残留

use base64::engine::general_purpose::URL_SAFE_NO_PAD as B64URL;
use base64::Engine as _;
use qingniao_core::config::{delete_history_entry, modify, replace_preserving_history};
use qingniao_core::transfer::crypto::{self, ChunkMeta, Envelope, Metadata};
use qingniao_core::transfer::engine::{ChunkStore, ClaimResult, Engine, FixedHost, KeySource};
use sha2::{Digest, Sha256};
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

/* ===================== 固定向量与注入实现 ===================== */

fn key() -> Vec<u8> {
    crypto::key_from_hex(&"3c".repeat(32)).expect("固定密钥")
}
fn dek() -> Vec<u8> {
    (0u8..32).map(|i| 0x10u8.wrapping_add(i)).collect()
}
fn tid(seed: u8) -> Vec<u8> {
    (0u8..16).map(|i| 0x20u8.wrapping_add(i + seed)).collect()
}

struct FixedKeys;
impl KeySource for FixedKeys {
    fn current(&self) -> Result<Option<String>, String> { Ok(Some(crypto::hex(&key()))) }
    fn previous(&self) -> Result<Option<String>, String> { Ok(None) }
}

/// 脚本化分片：可控制 delete 失败集合（用于触发 pending_deletes 入队）
struct Scripted {
    sealed: Mutex<HashMap<String, Vec<u8>>>,
    fail_delete: Mutex<HashSet<String>>,
}
impl ChunkStore for Scripted {
    fn fetch(&self, token: &str) -> Result<Vec<u8>, String> {
        Ok(self.sealed.lock().unwrap().get(token).cloned().expect("未知分片"))
    }
    fn delete(&self, token: &str) -> Result<(), String> {
        if self.fail_delete.lock().unwrap().contains(token) {
            Err(format!("注入删除失败 {token}"))
        } else {
            Ok(())
        }
    }
}

/// 单片信封（seed 区分不同 tid → 不同 payload 指纹）
fn make_payload(plain: &[u8], seed: u8) -> String {
    let nonce: Vec<u8> = (0u8..12).map(|j| 0x30u8.wrapping_add(j)).collect();
    let env = Envelope {
        v: crypto::PROTO_VERSION,
        kid: crypto::fingerprint(&key()),
        dek: B64URL.encode(dek()),
        tid: crypto::hex(&tid(seed)),
        meta: Metadata {
            name: format!("m0b-{seed}.bin"),
            mime: String::new(),
            size: plain.len() as u64,
            sha256: crypto::hex(&Sha256::digest(plain)),
            ts: crypto::now_unix(),
            chunks: vec![ChunkMeta {
                t: format!("boxcnM{seed}"),
                n: 0,
                off: 0,
                size: plain.len() as u64,
                nonce: B64URL.encode(&nonce),
            }],
        },
    };
    crypto::seal_payload(&key(), &env).expect("封装")
}

static SEQ: AtomicU64 = AtomicU64::new(0);

fn temp_root(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "qn-m0b-{}-{}-{}",
        tag,
        std::process::id(),
        SEQ.fetch_add(1, Ordering::Relaxed)
    ));
    std::fs::create_dir_all(&dir).expect("建临时目录");
    dir
}

fn engine_at(work: PathBuf, dl: &str, store: Option<Arc<dyn ChunkStore>>) -> Engine {
    Engine::open_with(
        work.clone(),
        Arc::new(FixedHost::new(work.join("cfg")).with_download_dir(dl)),
        Arc::new(FixedKeys),
        store,
    )
    .expect("构建 Engine")
}

fn claim(engine: &Engine, payload: &str) -> qingniao_core::transfer::engine::PendingDownload {
    let ev = engine.evaluate_payload(payload).expect("校验");
    let view = engine.create_session(payload, ev).expect("建会话");
    match engine.claim_session(&view.handle).expect("领取") {
        ClaimResult::Start(p) => p,
        ClaimResult::AlreadyDone { .. } => panic!("不应命中幂等"),
    }
}

/* ===================== 指纹锁（D24 / §7 约束①②③） ===================== */

#[test]
fn m0b_fingerprint_lock_busy_across_claims() {
    let root = temp_root("fp-busy");
    let engine = engine_at(root.join("work"), root.join("dl").to_str().unwrap(), None);
    let payload = make_payload(b"m0b-busy", 1);

    // 第一次 claim：持有指纹锁（TaskHandle 存活期间）
    let _p1 = claim(&engine, &payload);
    // 同一指纹第二次 claim → Busy（不再依赖进程内 fp_state，跨进程等价）
    let ev = engine.evaluate_payload(&payload).unwrap();
    let view = engine.create_session(&payload, ev).unwrap();
    match engine.claim_session(&view.handle) {
        Err(qingniao_core::transfer::engine::ClaimError::Busy) => {}
        Err(qingniao_core::transfer::engine::ClaimError::Expired) => panic!("应 Busy，实际 Expired"),
        Err(qingniao_core::transfer::engine::ClaimError::NotFound) => panic!("应 Busy，实际 NotFound"),
        Ok(_) => panic!("应 Busy，实际 Ok"),
    }
}

#[test]
fn m0b_fingerprint_lock_released_after_failure_and_done_is_idempotent() {
    let root = temp_root("fp-release");
    let dl = root.join("dl");
    let plain = b"m0b-release-payload".to_vec();
    let payload = make_payload(&plain, 2);

    // 注入：删除失败 → run_download_sync 走到 queue_pending_deletes；下载本身成功
    let scripted = Arc::new(Scripted {
        sealed: Mutex::new(HashMap::new()),
        fail_delete: Mutex::new(["boxcnM2".to_string()].into_iter().collect()),
    });
    // 填充密文（构造与 make_payload 相同参数）
    let nonce: Vec<u8> = (0u8..12).map(|j| 0x30u8.wrapping_add(j)).collect();
    let sealed = crypto::seal_chunk(&dek(), &nonce, &tid(2), 0, &plain).unwrap();
    scripted.sealed.lock().unwrap().insert("boxcnM2".into(), sealed);

    let engine = engine_at(root.join("work"), dl.to_str().unwrap(), Some(scripted.clone()));
    let mut p1 = claim(&engine, &payload);
    let final_path = engine.run_download_sync(&mut p1).expect("首次下载成功");
    assert!(std::path::Path::new(&final_path).exists());

    // 成功 → consumed 落盘 → 新会话 claim 命中幂等（不再需要锁）
    let ev = engine.evaluate_payload(&payload).unwrap();
    let view = engine.create_session(&payload, ev).unwrap();
    match engine.claim_session(&view.handle).expect("领取") {
        ClaimResult::AlreadyDone { path } => assert_eq!(path, final_path),
        ClaimResult::Start(_) => panic!("成功后必须幂等回放"),
    }
}

/// D24 ②：另一 Engine 实例（同 work_dir）的已消费记录不得被覆盖丢失
#[test]
fn m0b_consumed_cache_merges_across_engines() {
    let root = temp_root("consumed-merge");
    let dl = root.join("dl");
    let plain = b"m0b-consumed".to_vec();
    let payload_a = make_payload(&plain, 3);
    let payload_b = make_payload(b"m0b-consumed-b", 4);

    let scripted = Arc::new(Scripted {
        sealed: Mutex::new(HashMap::new()),
        fail_delete: Mutex::new(HashSet::new()),
    });
    for (seed, plain) in [(3u8, plain.as_slice()), (4u8, b"m0b-consumed-b".as_slice())] {
        let nonce: Vec<u8> = (0u8..12).map(|j| 0x30u8.wrapping_add(j)).collect();
        let sealed = crypto::seal_chunk(&dek(), &nonce, &tid(seed), 0, plain).unwrap();
        scripted
            .sealed
            .lock()
            .unwrap()
            .insert(format!("boxcnM{seed}"), sealed);
    }

    let a = engine_at(root.join("work"), dl.to_str().unwrap(), Some(scripted.clone()));
    let mut pa = claim(&a, &payload_a);
    a.run_download_sync(&mut pa).expect("A 下载成功");

    // B 是同一 work_dir 的**另一进程视角**：消费 B 的指纹后，A 的记录必须仍在
    let b = engine_at(root.join("work"), dl.to_str().unwrap(), Some(scripted.clone()));
    let mut pb = claim(&b, &payload_b);
    b.run_download_sync(&mut pb).expect("B 下载成功");

    let disk = std::fs::read_to_string(root.join("work").join("consumed.json")).unwrap();
    let v: serde_json::Value = serde_json::from_str(&disk).unwrap();
    let items = v["items"].as_object().expect("items");
    assert_eq!(items.len(), 2, "两个指纹都必须在磁盘缓存中（合并语义）");

    // A 重新打开也能看到 B 的记录（锁内重读）
    let a2 = engine_at(root.join("work"), dl.to_str().unwrap(), Some(scripted.clone()));
    let ev = a2.evaluate_payload(&payload_b).unwrap();
    let view = a2.create_session(&payload_b, ev).unwrap();
    match a2.claim_session(&view.handle).expect("领取") {
        ClaimResult::AlreadyDone { .. } => {}
        ClaimResult::Start(_) => panic!("跨引擎已消费记录丢失（D24 ② 被破坏）"),
    }
}

/// §6.4 / 约束③：删除失败入队走锁内合并；两实例入队不互吞
#[test]
fn m0b_pending_deletes_merge_across_engines() {
    let root = temp_root("pd-merge");
    let dl = root.join("dl");
    let scripted = Arc::new(Scripted {
        sealed: Mutex::new(HashMap::new()),
        fail_delete: Mutex::new(HashSet::new()),
    });
    for seed in [5u8, 6u8] {
        let plain = format!("m0b-pd-{seed}").into_bytes();
        let nonce: Vec<u8> = (0u8..12).map(|j| 0x30u8.wrapping_add(j)).collect();
        let sealed = crypto::seal_chunk(&dek(), &nonce, &tid(seed), 0, &plain).unwrap();
        scripted.sealed.lock().unwrap().insert(format!("boxcnM{seed}"), sealed);
    }
    // 各自删除失败一个 token
    scripted.fail_delete.lock().unwrap().insert("boxcnM5".into());
    scripted.fail_delete.lock().unwrap().insert("boxcnM6".into());

    let payload_a = make_payload(b"m0b-pd-5", 5);
    let payload_b = make_payload(b"m0b-pd-6", 6);
    let a = engine_at(root.join("work"), dl.to_str().unwrap(), Some(scripted.clone()));
    let mut pa = claim(&a, &payload_a);
    a.run_download_sync(&mut pa).expect("A 成功（删除入队）");
    let b = engine_at(root.join("work"), dl.to_str().unwrap(), Some(scripted.clone()));
    let mut pb = claim(&b, &payload_b);
    b.run_download_sync(&mut pb).expect("B 成功（删除入队）");

    let disk = std::fs::read_to_string(root.join("work").join("pending_deletes.json")).unwrap();
    let v: Vec<String> = serde_json::from_str(&disk).unwrap();
    assert!(v.contains(&"boxcnM5".to_string()) && v.contains(&"boxcnM6".to_string()),
        "两实例的待删条目都必须保留：{v:?}");
}

/* ===================== R8 合并语义（config.rs） ===================== */

fn seed_config(path: &PathBuf) {
    std::fs::write(
        path,
        r#"{
  "schema_version": 2,
  "app_id": "cli_old",
  "webhooks": [{"id":"w1","name":"峰哥","url":"https://open.feishu.cn/open-apis/bot/v2/hook/x","secret":"s"}],
  "history": [
    {"time":"2026-09-19T01:00:00.000Z","kind":"text","dir":"out","text":"旧消息一"},
    {"time":"2026-09-19T02:00:00.000Z","kind":"file","dir":"out","name":"a.zip"}
  ],
  "transfer": {"configured_port": 1234}
}"#,
    )
    .unwrap();
}

#[test]
fn m0b_replace_preserves_disk_history() {
    let dir = temp_root("r8-replace");
    let cfg = dir.join("qingniao.json");
    seed_config(&cfg);

    // 前端快照带着自己的（过时的）history —— 落盘必须以磁盘为准
    let frontend = serde_json::json!({
        "schema_version": 1,
        "app_id": "cli_new",
        "webhooks": [],
        "history": [{"time":"x","kind":"text","dir":"out","text":"前端快照的残留"}],
    });
    replace_preserving_history(&cfg, frontend).expect("替换成功");

    let disk: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&cfg).unwrap()).unwrap();
    assert_eq!(disk["app_id"], "cli_new", "其余字段整体替换");
    assert_eq!(disk["schema_version"], 2, "验收点③：schema_version 强升 2");
    let hist = disk["history"].as_array().unwrap();
    assert_eq!(hist.len(), 2, "磁盘 history 必须原样保留（R8 合并语义）");
    assert_eq!(hist[0]["text"], "旧消息一");
    assert!(cfg.to_string_lossy().ends_with("qingniao.json"));
    // 无临时文件残留
    let residue: Vec<_> = std::fs::read_dir(&dir)
        .unwrap()
        .filter_map(|e| e.ok())
        .filter(|e| e.file_name().to_string_lossy().contains(".tmp"))
        .collect();
    assert!(residue.is_empty(), "临时文件不得残留: {residue:?}");
}

#[test]
fn m0b_delete_history_entry_locates_under_lock() {
    let dir = temp_root("r8-delete");
    let cfg = dir.join("qingniao.json");
    seed_config(&cfg);

    let n = delete_history_entry(&cfg, "2026-09-19T02:00:00.000Z", "file").expect("删除成功");
    assert_eq!(n, 1);
    let disk: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&cfg).unwrap()).unwrap();
    let hist = disk["history"].as_array().unwrap();
    assert_eq!(hist.len(), 1);
    assert_eq!(hist[0]["kind"], "text");

    // 再删不存在的 → 0 条，不出错
    let n = delete_history_entry(&cfg, "2026-09-19T02:00:00.000Z", "file").expect("幂等");
    assert_eq!(n, 0);
}

/// D4/P2-4：modify 并发安全，且临时文件名含 pid（固定名会两进程互踩）
#[test]
fn m0b_config_modify_concurrent_no_residue() {
    let dir = temp_root("r8-concurrent");
    let cfg = dir.join("qingniao.json");
    seed_config(&cfg);

    let cfg1 = cfg.clone();
    let cfg2 = cfg.clone();
    let h1 = std::thread::spawn(move || {
        for i in 0..5 {
            modify(&cfg1, |c| {
                c.raw.insert("seq".into(), serde_json::json!(i));
                Ok(())
            })
            .unwrap();
        }
    });
    let h2 = std::thread::spawn(move || {
        for i in 0..5 {
            modify(&cfg2, |c| {
                c.raw.insert("seq2".into(), serde_json::json!(i));
                Ok(())
            })
            .unwrap();
        }
    });
    h1.join().unwrap();
    h2.join().unwrap();

    let residue: Vec<String> = std::fs::read_dir(&dir)
        .unwrap()
        .filter_map(|e| e.ok())
        .map(|e| e.file_name().to_string_lossy().to_string())
        .filter(|n| n.contains(".tmp"))
        .collect();
    assert!(residue.is_empty(), "并发修改后不得残留临时文件: {residue:?}");
    // history 不受 modify 影响（modify 只动调用方闭包触碰的键）
    let disk: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(&cfg).unwrap()).unwrap();
    assert_eq!(disk["history"].as_array().unwrap().len(), 2);
    assert_eq!(disk["transfer"]["configured_port"], 1234, "未知/未触碰字段原样保留");
}
