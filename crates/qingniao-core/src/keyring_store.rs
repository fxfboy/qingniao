//! OS 凭据库封装（D15/D21，设计文档 §12.3）。
//!
//! 服务名 `com.qingniao.transfer`；账户名：
//! - `master-key`：当前主密钥 K（64 字符 hex）
//! - `master-key-previous`：轮换过渡的旧密钥（保留 30 天）
//! - `master-key-previous-at`：旧密钥降级时间（Unix 秒，十进制字符串）

use crate::transfer::crypto;

/// 旧密钥保留期 30 天（§12.3）
pub const PREVIOUS_TTL_SECS: i64 = 30 * 24 * 3600;

const SERVICE: &str = "com.qingniao.transfer";
const ACCOUNT_CURRENT: &str = "master-key";
const ACCOUNT_PREVIOUS: &str = "master-key-previous";
const ACCOUNT_PREVIOUS_AT: &str = "master-key-previous-at";

fn entry(account: &str) -> Result<keyring::Entry, String> {
    keyring::Entry::new(SERVICE, account).map_err(|e| format!("凭据库不可用: {e}"))
}

fn get(account: &str) -> Result<Option<String>, String> {
    match entry(account)?.get_password() {
        Ok(v) => Ok(Some(v)),
        Err(keyring::Error::NoEntry) => Ok(None),
        Err(e) => Err(format!("读取凭据库失败({account}): {e}")),
    }
}

fn set(account: &str, value: &str) -> Result<(), String> {
    entry(account)?
        .set_password(value)
        .map_err(|e| format!("写入凭据库失败({account}): {e}"))
}

fn delete(account: &str) -> Result<(), String> {
    match entry(account)?.delete_credential() {
        Ok(()) => Ok(()),
        Err(keyring::Error::NoEntry) => Ok(()),
        Err(e) => Err(format!("删除凭据失败({account}): {e}")),
    }
}

/* ===================== 主密钥 ===================== */

/// 读取当前主密钥（hex 64 字符）；未配置返回 None
pub fn get_current() -> Result<Option<String>, String> {
    get(ACCOUNT_CURRENT)
}

/// 写入当前主密钥（hex 64 字符；不做格式校验，调用方负责）
pub fn set_current(hex_key: &str) -> Result<(), String> {
    set(ACCOUNT_CURRENT, hex_key)
}

/// 读取旧密钥（若存在且未过 30 天保留期）；过期则顺手清理并返回 None
pub fn get_previous() -> Result<Option<String>, String> {
    let prev = match get(ACCOUNT_PREVIOUS)? {
        Some(v) => v,
        None => return Ok(None),
    };
    let at: i64 = match get(ACCOUNT_PREVIOUS_AT)? {
        Some(s) => s.parse().unwrap_or(0),
        None => 0,
    };
    let now = crypto::now_unix();
    if at > 0 && now - at > PREVIOUS_TTL_SECS {
        // 保留期满：删除旧密钥，此后旧 payload 全部失效（§12.3）
        let _ = delete(ACCOUNT_PREVIOUS);
        let _ = delete(ACCOUNT_PREVIOUS_AT);
        return Ok(None);
    }
    Ok(Some(prev))
}

/// 轮换：旧密钥降级为 previous 并记录时间，写入新密钥（§12.3）
pub fn rotate(new_hex: &str) -> Result<(), String> {
    if let Some(old) = get(ACCOUNT_CURRENT)? {
        set(ACCOUNT_PREVIOUS, &old)?;
        set(ACCOUNT_PREVIOUS_AT, &crypto::now_unix().to_string())?;
    }
    set(ACCOUNT_CURRENT, new_hex)
}

/// 导入（覆盖本机密钥；配对页确认指纹一致后调用）
pub fn import(hex_key: &str) -> Result<(), String> {
    set(ACCOUNT_CURRENT, hex_key)
}

/// 生成新密钥（CSPRNG 32 字节 → hex 64 字符）
pub fn generate() -> String {
    crypto::hex(&crypto::random_bytes(32))
}

/// 清除本机全部传输密钥（导入覆盖不需要显式清除，此处供「重新生成」等场景用）
pub fn clear_previous() -> Result<(), String> {
    delete(ACCOUNT_PREVIOUS)?;
    delete(ACCOUNT_PREVIOUS_AT)
}

/* ===================== 测试（不触碰真实凭据库） ===================== */
#[cfg(test)]
mod tests {
    use super::*;
    use crate::transfer::crypto;

    #[test]
    fn generated_key_is_valid_hex64() {
        let k = generate();
        assert_eq!(k.len(), 64);
        assert!(crypto::key_from_hex(&k).is_ok());
        assert_eq!(crypto::key_from_hex(&k).unwrap().len(), 32);
    }

    #[test]
    fn key_from_hex_rejects_bad_input() {
        assert!(crypto::key_from_hex("abc").is_err());
        assert!(crypto::key_from_hex(&"g".repeat(64)).is_err());
        assert!(crypto::key_from_hex(&"a".repeat(63)).is_err());
    }
}
