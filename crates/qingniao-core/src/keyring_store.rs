//! OS 凭据库封装（D15/D21）。
//! 服务名 `com.qingniao.transfer`；账户 `master-key` 保存当前主密钥 K（64 字符 hex）。

use crate::transfer::crypto;

const SERVICE: &str = "com.qingniao.transfer";
const ACCOUNT_CURRENT: &str = "master-key";

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

/* ===================== 主密钥 ===================== */

/// 读取当前主密钥（hex 64 字符）；未配置返回 None
pub fn get_current() -> Result<Option<String>, String> {
    get(ACCOUNT_CURRENT)
}

/// 替换当前主密钥（hex 64 字符；不做格式校验，调用方负责）。新解析链接只接受替换后的密钥。
pub fn set_current(hex_key: &str) -> Result<(), String> {
    set(ACCOUNT_CURRENT, hex_key)
}

/// 导入并替换当前主密钥；配对页确认指纹一致后调用，新解析链接只接受替换后的密钥。
pub fn import(hex_key: &str) -> Result<(), String> {
    set(ACCOUNT_CURRENT, hex_key)
}

/// 生成新密钥（CSPRNG 32 字节 → hex 64 字符）
pub fn generate() -> String {
    crypto::hex(&crypto::random_bytes(32))
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
