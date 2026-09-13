//! U6：篡改密文必须被拒（M0a 出口 3，纯本地）。
//!
//! 覆盖 payload 与分片两个方向、以及 GCM tag 的三种触发面：
//! 密文位翻转、AAD 变更（tid / 片序号）、密钥或 nonce 不符、密文截断。
//!
//! 全部用例自建向量（固定 K/DEK/tid/nonce），不依赖 keyring、不读写磁盘、
//! 不碰真实租户——因此可以在 M0a 阶段跑。

use base64::engine::general_purpose::URL_SAFE_NO_PAD as B64URL;
use base64::Engine as _;
use qingniao_core::transfer::crypto::{self, ChunkMeta, Envelope, Metadata};
use sha2::{Digest, Sha256};

fn key() -> Vec<u8> {
    crypto::key_from_hex(&"3c".repeat(32)).expect("固定密钥")
}
fn dek() -> Vec<u8> {
    (0u8..32).map(|i| 0x10u8.wrapping_add(i)).collect()
}
fn tid() -> Vec<u8> {
    (0u8..16).map(|i| 0x20u8.wrapping_add(i)).collect()
}
fn nonce() -> Vec<u8> {
    (0u8..12).map(|i| 0x30u8.wrapping_add(i)).collect()
}

fn envelope_with(plain: &[u8]) -> Envelope {
    Envelope {
        v: crypto::PROTO_VERSION,
        kid: crypto::fingerprint(&key()),
        dek: B64URL.encode(dek()),
        tid: crypto::hex(&tid()),
        meta: Metadata {
            name: "tamper.bin".into(),
            mime: String::new(),
            size: plain.len() as u64,
            sha256: crypto::hex(&Sha256::digest(plain)),
            ts: crypto::now_unix(),
            chunks: vec![ChunkMeta {
                t: "boxcnTAMPER".into(),
                n: 0,
                off: 0,
                size: plain.len() as u64,
                nonce: B64URL.encode(nonce()),
            }],
        },
    }
}

/* ===================== payload ===================== */

#[test]
fn tampered_payload_is_rejected() {
    let plain = b"hello-qingniao".to_vec();
    let env = envelope_with(&plain);
    let payload = crypto::seal_payload(&key(), &env).expect("封装");

    // 基线：未篡改可解
    crypto::open_payload(&key(), &payload).expect("未篡改必须可解");

    // 位翻转（逐字节各翻一位太长，覆盖首/中/末三处即可代表整段）
    let raw = B64URL
        .decode(&payload)
        .expect("decode");
    for &idx in &[0usize, raw.len() / 2, raw.len() - 1] {
        let mut t = raw.clone();
        t[idx] ^= 0x01;
        let s = B64URL.encode(&t);
        assert!(
            crypto::open_payload(&key(), &s).is_err(),
            "第 {idx} 字节被翻转后仍能解开——GCM tag 校验未生效"
        );
    }

    // 截断（去掉 tag 的一部分）
    let s = B64URL.encode(&raw[..raw.len() - 1]);
    assert!(crypto::open_payload(&key(), &s).is_err(), "截断后仍能解开");

    // 换密钥
    let other = crypto::key_from_hex(&"4d".repeat(32)).expect("另一把密钥");
    assert!(crypto::open_payload(&other, &payload).is_err(), "换密钥后仍能解开");

    // 非 base64url
    assert!(crypto::open_payload(&key(), "!!!not-base64!!!").is_err());
}

/* ===================== 分片 ===================== */

#[test]
fn tampered_chunk_is_rejected() {
    let plain: Vec<u8> = (0..256u32).map(|i| i as u8).collect();
    let sealed = crypto::seal_chunk(&dek(), &nonce(), &tid(), 0, &plain).expect("加密");

    // 基线
    assert_eq!(
        crypto::open_chunk(&dek(), &nonce(), &tid(), 0, &sealed).expect("可解"),
        plain
    );

    // 位翻转（含 tag 区）
    for &idx in &[0usize, sealed.len() / 2, sealed.len() - 1] {
        let mut t = sealed.clone();
        t[idx] ^= 0x01;
        assert!(
            crypto::open_chunk(&dek(), &nonce(), &tid(), 0, &t).is_err(),
            "第 {idx} 字节被翻转后仍能解开"
        );
    }

    // 密文截断到 tag 之内（长度不合法）
    assert!(crypto::open_chunk(&dek(), &nonce(), &tid(), 0, &sealed[..8]).is_err());

    // AAD 变更：片序号不同 → tag 失败（这是 AAD 绑定片序的证据）
    assert!(
        crypto::open_chunk(&dek(), &nonce(), &tid(), 1, &sealed).is_err(),
        "把 n=0 的密文当 n=1 解开——AAD 未绑定片序号"
    );
    // AAD 变更：tid 不同 → 拒绝（防跨传输替换）
    let other_tid: Vec<u8> = (0u8..16).map(|i| 0x99u8.wrapping_add(i)).collect();
    assert!(
        crypto::open_chunk(&dek(), &nonce(), &other_tid, 0, &sealed).is_err(),
        "换 tid 后仍能解开——AAD 未绑定所属传输"
    );

    // 密钥 / nonce 不符
    let other_dek: Vec<u8> = (0u8..32).map(|i| 0x77u8.wrapping_add(i)).collect();
    let other_nonce: Vec<u8> = (0u8..12).map(|i| 0x88u8.wrapping_add(i)).collect();
    assert!(crypto::open_chunk(&other_dek, &nonce(), &tid(), 0, &sealed).is_err());
    assert!(crypto::open_chunk(&dek(), &other_nonce, &tid(), 0, &sealed).is_err());
}

/* ===================== 元数据自洽（§7.5 第 3 步） ===================== */

#[test]
fn tampered_metadata_is_rejected() {
    let plain: Vec<u8> = (0..64u32).map(|i| i as u8).collect();
    let good = envelope_with(&plain).meta;
    crypto::validate_metadata(&good).expect("基线必须自洽");

    // 片序号错乱
    let mut m = good.clone();
    m.chunks[0].n = 1;
    assert!(crypto::validate_metadata(&m).is_err(), "n 被改成 1 后仍通过");

    // 偏移不连续
    let mut m = good.clone();
    m.chunks[0].off = 1;
    assert!(crypto::validate_metadata(&m).is_err(), "off 被改成 1 后仍通过");

    // size 与片 size 不符
    let mut m = good.clone();
    m.chunks[0].size = 63;
    assert!(crypto::validate_metadata(&m).is_err(), "片 size 被改小后仍通过");

    // sha256 被改
    let mut m = good.clone();
    m.sha256 = "0".repeat(64);
    // 注：sha256 长度仍合法，validate 只查长度不查内容——这条**应当通过**，
    // 内容比对发生在合并完成之后（§7.5 第 6 步），不在本函数职责内。
    crypto::validate_metadata(&m).expect("validate_metadata 只查字段形态，不比对内容");

    // 长度非法的 sha256
    let mut m = good.clone();
    m.sha256 = "abc".into();
    assert!(crypto::validate_metadata(&m).is_err());

    // 超过 100 MB 上限
    let mut m = good.clone();
    m.size = crypto::MAX_FILE_SIZE + 1;
    assert!(crypto::validate_metadata(&m).is_err(), "超上限未被拒绝");

    // 文件名缺失 / ts 缺失
    let mut m = good.clone();
    m.name = String::new();
    assert!(crypto::validate_metadata(&m).is_err());
    let mut m = good.clone();
    m.ts = 0;
    assert!(crypto::validate_metadata(&m).is_err());

    // 分片列表为空
    let mut m = good.clone();
    m.chunks.clear();
    assert!(crypto::validate_metadata(&m).is_err());
}
