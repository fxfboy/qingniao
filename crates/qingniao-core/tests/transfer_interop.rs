//! 跨版本互操作 + 加密确定性 fixture 的消费测试（方案 §10 M0a 出口 1 / 出口 2a）。
//!
//! fixture 由 `src-tauri/examples/gen_transfer_fixture.rs` 在**重构前**生成，本文件**只读**。
//! 它证明两件事：
//! 1. **跨版本（解密方向）**：新代码能解开重构前产出的 payload 与分片密文，得到同一 metadata 与同一明文；
//! 2. **加密方向未被改坏**：`seal_chunk` 全部入参皆为参数、体内无 RNG，因此可以对冻结密文做**逐字节**断言。
//!
//! 为什么这两条必须都有：只测解密方向的话，若重构改坏了**封装**侧（AAD 拼接顺序、`n` 字节序、
//! nonce 处理），新代码「自己封 → 自己解」自洽，而「解旧 fixture」走的是没被改坏的解密路径，照样全绿。

use base64::engine::general_purpose::URL_SAFE_NO_PAD as B64URL;
use base64::Engine as _;
use qingniao_core::transfer::crypto::{self, Envelope};
use serde_json::Value;

fn fixture() -> Value {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("..")
        .join("tests")
        .join("golden")
        .join("transfer-interop.json");
    let raw = std::fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("读取 fixture 失败（{}）: {e}", path.display()));
    serde_json::from_str(&raw).expect("fixture 必须是合法 JSON")
}

fn b64(s: &str) -> Vec<u8> {
    B64URL.decode(s).expect("fixture 里的 base64url 必须合法")
}

/// fixture 里没有公开的 hex 解码入口（`crypto::hex_decode` 是私有的），这里自带一个最小实现
fn hex_decode(s: &str) -> Vec<u8> {
    assert!(s.len() % 2 == 0, "hex 长度必须是偶数");
    (0..s.len() / 2)
        .map(|i| u8::from_str_radix(&s[i * 2..i * 2 + 2], 16).expect("hex 必须合法"))
        .collect()
}

/// 从信封派生分片加密所需的三件套（fixture 自包含：不需要额外的字段）
fn key_material(f: &Value) -> (Vec<u8>, Vec<u8>, Vec<u8>) {
    let env: Envelope = serde_json::from_value(f["envelope"].clone()).expect("信封可反序列化");
    let dek = b64(&env.dek);
    let tid = hex_decode(&env.tid);
    assert_eq!(dek.len(), 32, "DEK 必须是 32 字节");
    assert_eq!(tid.len(), 16, "tid 必须是 16 字节");
    (dek, tid, vec![])
}

#[test]
fn payload_decrypts_across_versions() {
    let f = fixture();
    let key = crypto::key_from_hex(f["key_hex"].as_str().expect("key_hex")).expect("固定密钥合法");
    let payload = f["payload_b64"].as_str().expect("payload_b64");

    // 1) 解冻结核密文 → 必须成功，且 metadata 与本文件记录的信封逐字段一致
    let env = crypto::open_payload(&key, payload).expect("重构后必须仍能解开重构前的 payload");
    assert_eq!(
        serde_json::to_value(&env).expect("序列化"),
        f["envelope"],
        "解出的信封与冻结样本不一致——wire 格式被改动"
    );
    // 2) 解出的 metadata 必须自洽
    crypto::validate_metadata(&env.meta).expect("解出的 metadata 必须自洽");
    // 3) 版本号与 kid 与冻结值一致
    assert_eq!(env.v, crypto::PROTO_VERSION);
    assert_eq!(env.kid, crypto::fingerprint(&key));
    // 4) 指纹算法未变（协议 §9.4 用它做单消费锁的键）
    assert_eq!(
        crypto::payload_fingerprint(payload),
        f["payload_fingerprint"].as_str().expect("payload_fingerprint"),
        "payload 指纹算法被改动——会破坏 D20 单消费锁与已消费缓存的键"
    );
}

#[test]
fn payload_seal_open_is_self_consistent() {
    let f = fixture();
    let key = crypto::key_from_hex(f["key_hex"].as_str().expect("key_hex")).expect("固定密钥合法");
    let want: Envelope = serde_json::from_value(f["envelope"].clone()).expect("信封可反序列化");

    // 封装方向：`seal_payload` 内含随机 nonce，因此不能逐字节比对；
    // 用「同一信封再封一次 → 解回来仍等于原信封」覆盖封装侧（与上一条的解密方向合起来两向都盖住）
    let again = crypto::seal_payload(&key, &want).expect("重新封装失败");
    assert_ne!(again, f["payload_b64"].as_str().unwrap(), "seal_payload 应因随机 nonce 而每次不同");
    let back = crypto::open_payload(&key, &again).expect("自封自解必须成功");
    assert_eq!(serde_json::to_value(&back).unwrap(), f["envelope"]);
}

#[test]
fn chunk_vectors_are_byte_exact_in_both_directions() {
    let f = fixture();
    let (dek, tid, _) = key_material(&f);
    let vectors = f["chunk_vectors"].as_array().expect("chunk_vectors 是数组");
    assert!(vectors.len() >= 4, "至少要覆盖 n = 0..3，才能钉住 AAD 里的片序号绑定");

    for v in vectors {
        let n = v["n"].as_u64().expect("n") as u32;
        let nonce = b64(v["nonce_b64"].as_str().expect("nonce_b64"));
        let plain = b64(v["plain_b64"].as_str().expect("plain_b64"));
        let sealed = b64(v["sealed_b64"].as_str().expect("sealed_b64"));

        // 解密方向
        let got = crypto::open_chunk(&dek, &nonce, &tid, n, &sealed)
            .unwrap_or_else(|e| panic!("n={n} 解密失败：{e}"));
        assert_eq!(got, plain, "n={n} 明文不一致");

        // 加密方向：seal_chunk 全为入参、无 RNG → 必须逐字节等于冻结密文
        let again = crypto::seal_chunk(&dek, &nonce, &tid, n, &plain).expect("重加密失败");
        assert_eq!(again, sealed, "n={n} 重加密字节不一致——AAD/加密实现被改动");
    }
}

#[test]
fn single_chunk_matches_envelope_meta() {
    let f = fixture();
    let (dek, tid, _) = key_material(&f);
    let env: Envelope = serde_json::from_value(f["envelope"].clone()).expect("信封可反序列化");
    assert_eq!(env.meta.chunks.len(), 1, "本 fixture 是合法单片信封");
    let c = &env.meta.chunks[0];

    let nonce = b64(&c.nonce);
    let plain = b64(f["single_chunk"]["plain_b64"].as_str().expect("plain_b64"));
    let sealed = b64(f["single_chunk"]["sealed_b64"].as_str().expect("sealed_b64"));

    // 信封里的 nonce 必须就是加密该片用的 nonce（否则下载端解不开）
    assert_eq!(plain.len() as u64, c.size, "分片 size 与明文长度不一致");
    assert_eq!(c.n, 0);
    assert_eq!(c.off, 0);

    assert_eq!(crypto::open_chunk(&dek, &nonce, &tid, 0, &sealed).expect("解密失败"), plain);
    assert_eq!(crypto::seal_chunk(&dek, &nonce, &tid, 0, &plain).expect("加密失败"), sealed);
}

#[test]
fn fixture_is_genuine_ciphertext() {
    // 防「断言空转」：若 fixture 被替换成明文或空串，上面的测试可能仍然通过。
    // 这里做最小证伪——篡改任意一字节必须导致解密失败。
    let f = fixture();
    let key = crypto::key_from_hex(f["key_hex"].as_str().expect("key_hex")).expect("固定密钥合法");
    let mut data = b64(f["payload_b64"].as_str().expect("payload_b64"));
    assert!(data.len() > 32, "payload 密文过短，样本可疑");
    let last = data.len() - 1;
    data[last] ^= 0x01;
    let tampered = B64URL.encode(&data);
    assert!(
        crypto::open_payload(&key, &tampered).is_err(),
        "篡改后的 payload 竟能解开——GCM tag 校验未生效或 fixture 不是真密文"
    );
}
