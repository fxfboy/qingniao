//! E2E 加密协议实现（设计文档 §7）。
//!
//! 结构：
//! - payload = `base64url( nonce(12B) ‖ AES-256-GCM(K, AAD="QN2:v1", JSON{v,kid,dek,meta}) ‖ tag(16B) )`
//! - 分片密文 = `AES-256-GCM(DEK, nonce_i, plaintext_i)`，AAD = `"QN2:v1" ‖ tid ‖ n`，
//!   云上仅存 `ciphertext ‖ tag`，nonce 存 metadata.chunks[i].nonce。
//!
//! 与文档 §7.3 的口径差异：分片 AAD 里的「payload 指纹」在实现上不可行——
//! payload JSON 含 chunk 的 file_token，而 file_token 要等分片上传完成后才知道，
//! 存在循环依赖。以每文件随机的 `tid`（16 字节 CSPRNG，hex 进 metadata）替代，
//! 绑定强度等价（防分片跨文件替换 / 错序）；payload 指纹仍按 §9.4 用于
//! 单消费锁 / 已消费缓存 / .part 命名，定义 = SHA-256(payload base64url 原文字节)。

use aes_gcm::aead::{Aead, KeyInit, Payload};
use aes_gcm::{Aes256Gcm, Nonce};
use base64::engine::general_purpose::URL_SAFE_NO_PAD as B64URL;
use base64::Engine as _;
use rand::RngCore;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

/// 协议版本 + AAD 前缀（防跨版本混淆，§7.3）
pub const PROTO_AAD: &[u8] = b"QN2:v1";
/// 协议版本号（payload JSON 的 v 字段）
pub const PROTO_VERSION: u32 = 1;
/// nonce 长度（GCM 标准 96 bit）
pub const NONCE_LEN: usize = 12;
/// tag 长度
pub const TAG_LEN: usize = 16;
/// 分片大小 16 MB（D17）
pub const CHUNK_SIZE: usize = 16 * 1024 * 1024;
/// 单文件程序上限 100 MB（D18，目标值；U15 实测前不承诺）
pub const MAX_FILE_SIZE: u64 = 100 * 1024 * 1024;

/* ===================== 密钥工具 ===================== */

/// SHA-256(K) 前 8 字节 hex = kid / 指纹（D19）
pub fn fingerprint(key32: &[u8]) -> String {
    let d = Sha256::digest(key32);
    hex(&d[..8])
}

/// hex（64 字符）→ 32 字节；用作 AES-256 密钥前必须解码（P2-1）
pub fn key_from_hex(s: &str) -> Result<Vec<u8>, String> {
    let s = s.trim();
    if s.len() != 64 || !s.chars().all(|c| c.is_ascii_hexdigit()) {
        return Err("主密钥必须是 64 位十六进制字符".into());
    }
    hex_decode(s).ok_or_else(|| "主密钥解码失败".to_string())
}

fn hex_decode(s: &str) -> Option<Vec<u8>> {
    let bytes = s.as_bytes();
    if bytes.len() % 2 != 0 {
        return None;
    }
    let mut out = Vec::with_capacity(bytes.len() / 2);
    let mut i = 0;
    while i < bytes.len() {
        let hi = (bytes[i] as char).to_digit(16)?;
        let lo = (bytes[i + 1] as char).to_digit(16)?;
        out.push((hi * 16 + lo) as u8);
        i += 2;
    }
    Some(out)
}

pub fn hex(data: &[u8]) -> String {
    data.iter().map(|b| format!("{:02x}", b)).collect()
}

/// CSPRNG 随机字节
pub fn random_bytes(n: usize) -> Vec<u8> {
    let mut buf = vec![0u8; n];
    rand::rngs::OsRng.fill_bytes(&mut buf);
    buf
}

/* ===================== payload 信封 ===================== */

/// metadata（payload 明文中的 meta 字段，D16）
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChunkMeta {
    /// 云空间 file_token
    pub t: String,
    /// 片序号（从 0）
    pub n: u32,
    /// 明文偏移
    pub off: u64,
    /// 明文分片大小
    pub size: u64,
    /// 分片加密 nonce（base64url 12 字节）
    pub nonce: String,
}

/// payload 明文 JSON
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Envelope {
    pub v: u32,
    /// 主密钥标识（SHA-256(K) 前 8 字节 hex）
    pub kid: String,
    /// 数据密钥（base64url 32 字节）
    pub dek: String,
    /// 随机传输 ID（hex 16 字节，绑定分片 AAD，见模块注释）
    pub tid: String,
    pub meta: Metadata,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Metadata {
    pub name: String,
    #[serde(default)]
    pub mime: String,
    pub size: u64,
    pub sha256: String,
    /// UTC Unix 秒（D8 新鲜度窗口）
    pub ts: i64,
    pub chunks: Vec<ChunkMeta>,
}

/// 加密 payload：返回 base64url 字符串（进链接 t= 参数）
pub fn seal_payload(key32: &[u8], env: &Envelope) -> Result<String, String> {
    let plain = serde_json::to_vec(env).map_err(|e| format!("payload 序列化失败: {e}"))?;
    let cipher = aes_gcm_encrypt(key32, &plain, PROTO_AAD)?;
    Ok(B64URL.encode(cipher))
}

/// 解密 payload：校验 AAD 与 v 版本号，返回 Envelope
pub fn open_payload(key32: &[u8], payload_b64: &str) -> Result<Envelope, String> {
    let data = B64URL
        .decode(payload_b64.trim())
        .map_err(|_| "payload 不是合法的 base64url".to_string())?;
    let plain = aes_gcm_decrypt(key32, &data, PROTO_AAD)?;
    let env: Envelope =
        serde_json::from_slice(&plain).map_err(|e| format!("payload 解析失败: {e}"))?;
    if env.v != PROTO_VERSION {
        return Err(format!("不支持的协议版本 v{}", env.v));
    }
    Ok(env)
}

/// payload 指纹 = SHA-256(payload base64url 原文字节)（§9.4）
pub fn payload_fingerprint(payload_b64: &str) -> String {
    hex(&Sha256::digest(payload_b64.trim().as_bytes()))
}

/// 按 kid 选择密钥：current → previous（§12.3）
/// `previous` 传 None 表示无旧密钥。
pub fn select_key(kid: &str, current_hex: &str, previous_hex: Option<&str>) -> Option<String> {
    let cur = key_from_hex(current_hex).ok()?;
    if fingerprint(&cur) == kid {
        return Some(current_hex.to_string());
    }
    if let Some(prev) = previous_hex {
        let prev_key = key_from_hex(prev).ok()?;
        if fingerprint(&prev_key) == kid {
            return Some(prev.to_string());
        }
    }
    None
}

/* ===================== 分片 ===================== */

/// 分片 AAD = `"QN2:v1" ‖ tid ‖ n(u32 LE)`（绑定传输与片序，防替换/错序）
fn chunk_aad(tid: &[u8], n: u32) -> Vec<u8> {
    let mut aad = Vec::with_capacity(PROTO_AAD.len() + tid.len() + 4);
    aad.extend_from_slice(PROTO_AAD);
    aad.extend_from_slice(tid);
    aad.extend_from_slice(&n.to_le_bytes());
    aad
}

/// 加密分片 → `ciphertext ‖ tag`
pub fn seal_chunk(dek32: &[u8], nonce12: &[u8], tid: &[u8], n: u32, plaintext: &[u8]) -> Result<Vec<u8>, String> {
    let aad = chunk_aad(tid, n);
    aes_gcm_encrypt_with_nonce(dek32, plaintext, &aad, nonce12)
}

/// 解密分片（GCM tag 校验失败 → Err）
pub fn open_chunk(dek32: &[u8], nonce12: &[u8], tid: &[u8], n: u32, sealed: &[u8]) -> Result<Vec<u8>, String> {
    let aad = chunk_aad(tid, n);
    aes_gcm_decrypt_with_nonce(dek32, sealed, &aad, nonce12)
}

/* ===================== AES-256-GCM 基础封装 ===================== */

fn cipher(key32: &[u8]) -> Result<Aes256Gcm, String> {
    Aes256Gcm::new_from_slice(key32).map_err(|_| "密钥长度必须为 32 字节".to_string())
}

/// 输出 = nonce(12B) ‖ ct ‖ tag(16B)
fn aes_gcm_encrypt(key32: &[u8], plain: &[u8], aad: &[u8]) -> Result<Vec<u8>, String> {
    let nonce = random_bytes(NONCE_LEN);
    let mut out = aes_gcm_encrypt_with_nonce(key32, plain, aad, &nonce)?;
    out.splice(0..0, nonce); // 前置 nonce
    Ok(out)
}

fn aes_gcm_decrypt(key32: &[u8], sealed_with_nonce: &[u8], aad: &[u8]) -> Result<Vec<u8>, String> {
    if sealed_with_nonce.len() < NONCE_LEN + TAG_LEN {
        return Err("密文长度不合法".into());
    }
    aes_gcm_decrypt_with_nonce(key32, &sealed_with_nonce[NONCE_LEN..], aad, &sealed_with_nonce[..NONCE_LEN])
}

fn aes_gcm_encrypt_with_nonce(key32: &[u8], plain: &[u8], aad: &[u8], nonce12: &[u8]) -> Result<Vec<u8>, String> {
    let c = cipher(key32)?;
    let n = Nonce::from_slice(nonce12);
    c.encrypt(n, Payload { msg: plain, aad })
        .map_err(|_| "加密失败".to_string())
}

fn aes_gcm_decrypt_with_nonce(key32: &[u8], sealed: &[u8], aad: &[u8], nonce12: &[u8]) -> Result<Vec<u8>, String> {
    let c = cipher(key32)?;
    let n = Nonce::from_slice(nonce12);
    c.decrypt(n, Payload { msg: sealed, aad })
        .map_err(|_| "解密失败（密钥不匹配或密文被篡改）".to_string())
}

/* ===================== 文件名净化（§9.3，P2-3） ===================== */

/// Windows 保留名（含带扩展名形式，如 CON.txt）
const WINDOWS_RESERVED: &[&str] = &[
    "CON", "PRN", "AUX", "NUL",
    "COM1", "COM2", "COM3", "COM4", "COM5", "COM6", "COM7", "COM8", "COM9",
    "LPT1", "LPT2", "LPT3", "LPT4", "LPT5", "LPT6", "LPT7", "LPT8", "LPT9",
];

/// 落盘前强制净化文件名：
/// NFC 规范化 → 剥离路径分隔符与 `..` → 过滤控制字符 → 禁止前导 `.` →
/// Windows 保留名处理 → 长度截断（≤255 字节，按字节）。
pub fn sanitize_filename(raw: &str) -> String {
    use unicode_normalization::UnicodeNormalization;
    // NFC 规范化（防等价形式绕过）
    let mut s: String = raw.nfc().collect();
    // 剥离路径分隔符与父目录片段
    for bad in ["..", "/", "\\", ":", "*", "?", "\"", "<", ">", "|"] {
        s = s.replace(bad, "_");
    }
    // 过滤控制字符（含 DEL）
    s = s.chars().filter(|c| !c.is_control()).collect();
    s = s.trim().to_string();
    // 禁止前导 '.'
    while s.starts_with('.') {
        s.remove(0);
    }
    if s.is_empty() {
        return "file".into();
    }
    // Windows 保留名：追加下划线
    let stem = s.split('.').next().unwrap_or("").to_ascii_uppercase();
    if WINDOWS_RESERVED.contains(&stem.as_str()) {
        s = format!("_{s}");
    }
    // 长度截断：≤255 字节（UTF-8 边界安全）
    while s.len() > 255 {
        s.pop();
    }
    if s.is_empty() { "file".into() } else { s }
}

/// 元数据自洽校验（§7.5 第 3 步）：字段齐全、off+size 连续覆盖 [0,size)、片数自洽
pub fn validate_metadata(meta: &Metadata) -> Result<(), String> {
    if meta.name.is_empty() {
        return Err("元数据缺少文件名".into());
    }
    if meta.sha256.len() != 64 {
        return Err("元数据 SHA-256 不合法".into());
    }
    if meta.ts <= 0 {
        return Err("元数据缺少时间戳".into());
    }
    if meta.chunks.is_empty() {
        return Err("元数据缺少分片列表".into());
    }
    if meta.size > MAX_FILE_SIZE {
        return Err("文件超出 100 MB 上限".into());
    }
    let expect_chunks = meta.size.div_ceil(CHUNK_SIZE as u64) as usize;
    if meta.chunks.len() != expect_chunks {
        return Err(format!("分片数不自洽：期望 {expect_chunks}，实际 {}", meta.chunks.len()));
    }
    let mut off: u64 = 0;
    for (i, c) in meta.chunks.iter().enumerate() {
        if c.n != i as u32 {
            return Err(format!("分片序号错乱：第 {i} 片 n={}", c.n));
        }
        if c.off != off {
            return Err(format!("分片偏移不连续：第 {i} 片 off={} 期望 {off}", c.off));
        }
        let expect_size = if i == meta.chunks.len() - 1 {
            meta.size - off
        } else {
            CHUNK_SIZE as u64
        };
        if c.size != expect_size {
            return Err(format!("分片大小不自洽：第 {i} 片 size={} 期望 {expect_size}", c.size));
        }
        off += c.size;
    }
    if off != meta.size {
        return Err(format!("分片未覆盖完整文件：覆盖 {off}，声明 {}", meta.size));
    }
    Ok(())
}

/* ===================== ts 新鲜度（D8） ===================== */

/// 默认新鲜度窗口 30 分钟
pub const FRESHNESS_WINDOW_SECS: i64 = 30 * 60;

pub fn is_fresh(ts: i64, now_unix: i64) -> bool {
    (now_unix - ts).abs() <= FRESHNESS_WINDOW_SECS
}

pub fn now_unix() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/* ===================== 测试 ===================== */
#[cfg(test)]
mod tests {
    use super::*;

    fn test_key() -> Vec<u8> {
        [7u8; 32].to_vec()
    }

    #[test]
    fn fingerprint_is_stable_hex_of_first_8_bytes() {
        let fp = fingerprint(&test_key());
        // D19：SHA-256(K) 前 8 字节 hex = 16 个十六进制字符
        assert_eq!(fp.len(), 16);
        assert!(fp.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn payload_roundtrip() {
        let key = test_key();
        let env = Envelope {
            v: PROTO_VERSION,
            kid: fingerprint(&key),
            dek: B64URL.encode(random_bytes(32)),
            tid: hex(&random_bytes(16)),
            meta: Metadata {
                name: "报告.pdf".into(),
                mime: "application/pdf".into(),
                size: 123456789,
                sha256: "a".repeat(64),
                ts: now_unix(),
                chunks: vec![],
            },
        };
        let sealed = seal_payload(&key, &env).unwrap();
        let opened = open_payload(&key, &sealed).unwrap();
        assert_eq!(opened.meta.name, "报告.pdf");
        assert_eq!(opened.meta.size, 123456789);
        // 篡改检测
        let mut tampered = sealed.clone().into_bytes();
        let last = tampered.len() - 1;
        tampered[last] ^= 1;
        assert!(open_payload(&key, &String::from_utf8(tampered).unwrap().as_str()).is_err());
        // 错误密钥
        assert!(open_payload(&[8u8; 32], &sealed).is_err());
    }

    #[test]
    fn chunk_roundtrip_and_aad_binding() {
        let dek = test_key();
        let nonce = random_bytes(NONCE_LEN);
        let tid = random_bytes(16);
        let plain = b"hello chunk 0";
        let sealed = seal_chunk(&dek, &nonce, &tid, 0, plain).unwrap();
        assert_eq!(open_chunk(&dek, &nonce, &tid, 0, &sealed).unwrap(), plain);
        // 换片序号 → AAD 不匹配 → 拒绝
        assert!(open_chunk(&dek, &nonce, &tid, 1, &sealed).is_err());
        // 换 tid → 拒绝
        let tid2 = random_bytes(16);
        assert!(open_chunk(&dek, &nonce, &tid2, 0, &sealed).is_err());
        // 篡改 → 拒绝
        let mut bad = sealed.clone();
        bad[0] ^= 1;
        assert!(open_chunk(&dek, &nonce, &tid, 0, &bad).is_err());
    }

    #[test]
    fn payload_fingerprint_differs_by_input() {
        assert_ne!(payload_fingerprint("abc"), payload_fingerprint("abd"));
        assert_eq!(payload_fingerprint("abc"), payload_fingerprint("abc"));
    }

    #[test]
    fn select_key_prefers_current_then_previous() {
        let cur = random_bytes(32);
        let prev = random_bytes(32);
        let cur_hex = hex(&cur);
        let prev_hex = hex(&prev);
        assert_eq!(select_key(&fingerprint(&cur), &cur_hex, Some(&prev_hex)), Some(cur_hex.clone()));
        assert_eq!(select_key(&fingerprint(&prev), &cur_hex, Some(&prev_hex)), Some(prev_hex.clone()));
        assert_eq!(select_key("deadbeef", &cur_hex, Some(&prev_hex)), None);
    }

    #[test]
    fn sanitize_handles_evil_names() {
        for raw in ["../../etc/passwd", "..\\..\\win.ini", "../..", "a/b\\c"] {
            let out = sanitize_filename(raw);
            // 关键安全性质：无路径分隔符、无父目录片段
            assert!(!out.contains('/'), "{out}");
            assert!(!out.contains('\\'), "{out}");
            assert!(!out.contains(".."), "{out}");
        }
        assert_eq!(sanitize_filename("CON.txt"), "_CON.txt");
        assert_eq!(sanitize_filename(".hidden"), "hidden");
        assert_eq!(sanitize_filename("bad\u{0007}name"), "badname");
        assert_eq!(sanitize_filename(""), "file");
        // NFC：等价形式归一
        let nfd = "caf\u{65}\u{301}";
        assert_eq!(sanitize_filename(nfd), "café");
        // 超长截断
        let long = "字".repeat(300);
        assert_eq!(sanitize_filename(&long).len(), 255);
    }

    #[test]
    fn metadata_validation() {
        let mk = |size: u64, chunks: Vec<(u64, u64)>| Metadata {
            name: "a".into(),
            mime: "".into(),
            size,
            sha256: "a".repeat(64),
            ts: now_unix(),
            chunks: chunks
                .into_iter()
                .enumerate()
                .map(|(i, (off, sz))| ChunkMeta {
                    t: format!("boxcn{i}"),
                    n: i as u32,
                    off,
                    size: sz,
                    nonce: String::new(),
                })
                .collect(),
        };
        // 单片
        assert!(validate_metadata(&mk(1024, vec![(0, 1024)])).is_ok());
        // 三片（16MB + 16MB + 8MB = 40MB）
        let cs = CHUNK_SIZE as u64;
        assert!(validate_metadata(&mk(cs * 2 + 8, vec![(0, cs), (cs, cs), (cs * 2, 8)])).is_ok());
        // 缺口
        assert!(validate_metadata(&mk(cs * 2, vec![(0, cs), (cs + 1, cs - 1)])).is_err());
        // 片数不符
        assert!(validate_metadata(&mk(cs * 2, vec![(0, cs * 2)])).is_err());
        // 超上限
        assert!(validate_metadata(&mk(MAX_FILE_SIZE + 1, vec![])).is_err());
    }

    #[test]
    fn freshness_window() {
        let now = now_unix();
        assert!(is_fresh(now - 60, now));
        assert!(!is_fresh(now - 31 * 60, now));
        assert!(!is_fresh(now + 31 * 60, now));
    }
}
