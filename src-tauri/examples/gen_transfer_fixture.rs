//! 一次性生成 CLI 文件传输的「跨版本互操作」与「加密确定性」fixture。
//!
//! **只在重构前运行一次**；产物提交入库后不得手工编辑（编辑即失去「重构前样本」的意义）。
//!
//! 运行：
//! ```bash
//! cargo run -p qingniao --example gen_transfer_fixture
//! ```
//!
//! 产物：`tests/golden/transfer-interop.json`
//!
//! 为什么分两部分：
//! - **单片合法信封**：走完整 `seal_payload` / `seal_chunk`，并过 `validate_metadata`。
//!   重构后断言「新代码能解出同一 metadata 与同一明文字节」，证明 wire 兼容。
//! - **分片向量**：`validate_metadata` 强制「非末片必须正好 CHUNK_SIZE(16 MB)」，
//!   所以一个合法的 4 片信封要 ~48 MB 明文，不可能入库。因此多片只保留
//!   `seal_chunk`/`open_chunk` 的字节级向量，覆盖 AAD 里片序号 `n` 的绑定。

use base64::engine::general_purpose::URL_SAFE_NO_PAD as B64URL;
use base64::Engine as _;
use qingniao_lib::transfer::crypto::{self, ChunkMeta, Envelope, Metadata};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::path::PathBuf;

/// 固定主密钥（64 hex = 32 字节）。**不得读 keyring**——否则 fixture 不可重放。
const KEY_HEX: &str = "000102030405060708090a0b0c0d0e0f101112131415161718191a1b1c1d1e1f";
/// 固定发送时间，落在新鲜度窗口内且便于人工核对
const TS: i64 = 1_776_000_000;
const FILE_NAME: &str = "回归样本-100.bin";
/// 生成时的 HEAD——写进 fixture 便于追溯「样本来自哪个版本的实现」
const FROM_COMMIT: &str = "cd3a7e4";

fn main() {
    let key = crypto::key_from_hex(KEY_HEX).expect("固定主密钥必须合法");

    // DEK 与 tid 也固定：分片向量要与单片信封用同一份参数，两个出口共用一套输入
    let dek: Vec<u8> = (0u8..32).map(|i| 0xA0u8.wrapping_add(i)).collect();
    let tid: Vec<u8> = (0u8..16).map(|i| 0x50u8.wrapping_add(i)).collect();

    // ---- (1) 单片合法信封 ----
    let plain: Vec<u8> = (0..4096u32).map(|i| ((i * 31) % 251) as u8).collect();
    let nonce: Vec<u8> = (0u8..12).map(|i| 0x70u8.wrapping_add(i)).collect();
    let sealed = crypto::seal_chunk(&dek, &nonce, &tid, 0, &plain).expect("分片加密失败");

    let env = Envelope {
        v: crypto::PROTO_VERSION,
        kid: crypto::fingerprint(&key),
        dek: B64URL.encode(&dek),
        tid: crypto::hex(&tid),
        meta: Metadata {
            name: FILE_NAME.to_string(),
            // 与现实现一致：engine 传空 mime（已知缺陷，见方案 §11-1），fixture 如实记录
            mime: String::new(),
            size: plain.len() as u64,
            sha256: crypto::hex(&Sha256::digest(&plain)),
            ts: TS,
            chunks: vec![ChunkMeta {
                t: "boxcnTESTSINGLE".to_string(),
                n: 0,
                off: 0,
                size: plain.len() as u64,
                nonce: B64URL.encode(&nonce),
            }],
        },
    };
    crypto::validate_metadata(&env.meta).expect("fixture 的信封必须自洽（否则样本本身不合法）");
    let payload = crypto::seal_payload(&key, &env).expect("信封加密失败");

    // ---- (2) 分片向量：覆盖 AAD 里 n 的绑定（n = 0..3）----
    let mut vectors: Vec<Value> = Vec::new();
    for n in 0..4u32 {
        let p: Vec<u8> = (0..64u32 + n)
            .map(|i| ((n as u8).wrapping_mul(17)).wrapping_add(i as u8))
            .collect();
        let nc: Vec<u8> = (0u8..12).map(|i| 0x90u8.wrapping_add(n as u8).wrapping_add(i)).collect();
        let s = crypto::seal_chunk(&dek, &nc, &tid, n, &p).expect("分片向量加密失败");
        vectors.push(json!({
            "n": n,
            "nonce_b64": B64URL.encode(&nc),
            "plain_b64": B64URL.encode(&p),
            "sealed_b64": B64URL.encode(&s),
        }));
    }

    let doc = json!({
        "note": "由 src-tauri/examples/gen_transfer_fixture.rs 在重构前生成，用于证明重构后 wire 字节与协议行为未变。**不得手工编辑**——编辑即失去「重构前样本」的意义，需重新从旧提交生成。",
        "generated_from_commit": FROM_COMMIT,
        "scheme": "payload = base64url(nonce_env(12) ‖ AES-256-GCM(K, \"QN2:v1\", JSON{dek,meta}) ‖ tag(16))；分片 = AES-256-GCM(DEK, nonce_i, plaintext)，AAD = \"QN2:v1\" ‖ tid ‖ n.to_le_bytes()",
        "key_hex": KEY_HEX,
        "payload_fingerprint": crypto::payload_fingerprint(&payload),
        "payload_b64": payload,
        "envelope": serde_json::to_value(&env).expect("信封序列化失败"),
        "single_chunk": {
            "plain_b64": B64URL.encode(&plain),
            "sealed_b64": B64URL.encode(&sealed),
        },
        "chunk_vectors": vectors,
    });

    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("tests")
        .join("golden")
        .join("transfer-interop.json");
    std::fs::create_dir_all(path.parent().expect("有父目录")).expect("建目录失败");
    std::fs::write(&path, serde_json::to_string_pretty(&doc).expect("序列化失败") + "\n")
        .expect("写 fixture 失败");

    println!("已写入 {}", path.display());
    println!("  payload_fingerprint = {}", crypto::payload_fingerprint(&payload));
    println!("  kid = {}", crypto::fingerprint(&key));
    println!("  单片明文 {} 字节，密文 {} 字节", plain.len(), sealed.len());
}
