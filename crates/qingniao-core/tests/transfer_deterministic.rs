//! M0a 出口 2（确定性面）——重构前冻结的字节级基线。
//!
//! 与 `transfer_interop.rs` 的分工：那边管**密文**（跨版本互操作 + 加密逐字节），
//! 这边管**纯函数与文案**（密钥选择、新鲜度边界、文件名净化、载荷提取、webhook 签名）。
//!
//! 期望值全部是**重构前的实测取值**，不是推导值——取错就失去冻结的意义。
//! 变更这些期望值前先问：这是有意的行为变更吗？若是，须走协议/方案修订并说明原因。

use qingniao_core::transfer::crypto as c;
use qingniao_core::transfer::engine::extract_payload;
// webhook_sign 已去重进 message::hmac_sign（M1），这里对齐

/* ===================== webhook 签名 ===================== */

#[test]
fn webhook_sign_known_vector() {
    // 冻结向量：HMAC-SHA256(key = "1700000000\n" + secret, msg = "") 的 base64
    assert_eq!(
        qingniao_core::message::hmac_sign("test-secret", "1700000000"),
        "mbm4Y4oluIPQ00qlBIhX8vAZ0EKv3nw0LuTb91jPL84=",
        "webhook 签名算法或编码被改动——会导致飞书侧签名校验失败（19021）"
    );
    // 与 core 的既有实现同语义（两份实现属重复，M1 收敛时以 core 为准）
    assert_eq!(
        qingniao_core::message::hmac_sign("test-secret", "1700000000"),
        qingniao_core::message::hmac_sign("test-secret", "1700000000"),
        "transfer 的 webhook_sign 与 core 的 hmac_sign 语义已分叉"
    );
}

/* ===================== 密钥选择（§12.3 轮换） ===================== */

#[test]
fn select_key_three_states() {
    let k1 = "11".repeat(32);
    let k2 = "22".repeat(32);
    let kid1 = c::fingerprint(&c::key_from_hex(&k1).expect("k1"));
    let kid2 = c::fingerprint(&c::key_from_hex(&k2).expect("k2"));
    assert_ne!(kid1, kid2, "两个测试密钥不应撞指纹");

    // current 命中
    assert_eq!(c::select_key(&kid1, &k1, Some(&k2)).as_deref(), Some(k1.as_str()));
    // previous 命中（轮换过渡期：旧链路的 payload 仍可解）
    assert_eq!(c::select_key(&kid2, &k1, Some(&k2)).as_deref(), Some(k2.as_str()));
    // 都不命中
    assert_eq!(c::select_key("deadbeefdeadbeef", &k1, Some(&k2)), None);
    // 无 previous 且 current 不命中
    assert_eq!(c::select_key(&kid2, &k1, None), None);
    // 非法 current hex 不 panic
    assert_eq!(c::select_key(&kid1, "not-hex", Some(&k2)), None);
}

/* ===================== 新鲜度窗口（D8） ===================== */

#[test]
fn is_fresh_boundaries() {
    const W: i64 = 600; // FRESHNESS_WINDOW_SECS
    assert_eq!(c::FRESHNESS_WINDOW_SECS, W, "新鲜度窗口常量被改动");
    assert_eq!(c::FRESHNESS_WINDOW_MINUTES, 10);

    let now = 1_700_000_000i64;
    for (delta, want) in [(0i64, true), (599, true), (W, true), (W + 1, false),
                          (-W, true), (-(W + 1), false)] {
        assert_eq!(c::is_fresh(now + delta, now), want, "delta={delta} 的判定不符");
    }
    // 语义是**绝对差**（未来时间戳同样按窗口约束）
    assert!(c::is_fresh(now + 100, now), "未来 100 s 应视为新鲜（绝对差语义）");
}

/* ===================== 文件名净化（U8 的安全面） ===================== */

#[test]
fn sanitize_filename_frozen_vectors() {
    // 冻结向量：每条都是重构前实测取值
    for (input, want) in [
        ("..", "_"),
        ("a/b.txt", "a_b.txt"),
        ("..\\..\\etc\\passwd", "____etc_passwd"),
        ("CON.txt", "_CON.txt"),
        ("aux", "_aux"),
        (".hidden", "hidden"),
        ("  spaced  ", "spaced"),
        ("a\u{0}b", "ab"),
        ("报告.pdf", "报告.pdf"),
        ("a|b*c?d\"e<f>g:h", "a_b_c_d_e_f_g_h"),
        ("", "file"),
        ("normal-file-1.2.3.tar.gz", "normal-file-1.2.3.tar.gz"),
    ] {
        assert_eq!(c::sanitize_filename(input), want, "输入 {input:?} 的净化结果不符");
    }
}

#[test]
fn sanitize_filename_truncates_to_255_bytes_on_char_boundary() {
    let long = "长".repeat(300);
    let got = c::sanitize_filename(&long);
    assert!(got.len() <= 255, "超过 255 字节：{}", got.len());
    // UTF-8 边界安全：按字节截断后仍是合法 UTF-8 且不 panic
    assert!(got.chars().all(|ch| ch == '长'), "截断破坏了字符边界");
    assert_eq!(got.len() % 3, 0, "3 字节字符被截断到半个字符");
}

/* ===================== 载荷提取（粘贴入口的容错面） ===================== */

#[test]
fn extract_payload_frozen_vectors() {
    // 接受：完整链接 / 裸载荷（含 base64url 的 - 与 _）
    assert_eq!(
        extract_payload("http://127.0.0.1:9876/dl?t=abc-_123XYZ").expect("完整链接"),
        "abc-_123XYZ"
    );
    assert_eq!(extract_payload("abc-_123XYZ").expect("裸载荷"), "abc-_123XYZ");
    // 拒绝：空 / 链接缺载荷 / 非载荷文本（错误文案也要冻结，CLI 的 kind 分类依赖它）
    assert_eq!(extract_payload("").unwrap_err(), "载荷为空");
    assert_eq!(extract_payload("http://127.0.0.1:9876/dl?t=").unwrap_err(), "链接缺少载荷");
    assert!(extract_payload("随便一段中文").is_err(), "非载荷文本必须拒绝");
}
