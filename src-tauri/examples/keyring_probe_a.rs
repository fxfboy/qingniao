//! M0a 出口 5 探针（A 侧二进制）：macOS unsigned 二进制跨二进制读 keychain 是否可行。
//!
//! # 安全约束
//!
//! **绝不触碰真实主密钥**。`keyring_store` 的 service/account 是私有常量且指向
//! `com.qingniao.transfer` / `master-key`（用户真实密钥所在），所以本探针**不使用**
//! `keyring_store::*`，而是用 `keyring` crate 直连一个独立的探针条目，机制相同
//! （同 crate、同 generic password、同访问模式），结论可迁移。
//!
//! 用法：`keyring_probe_a write|read|delete`

const SERVICE: &str = "com.qingniao.transfer.PROBE";
const ACCOUNT: &str = "probe-key-A";

fn main() {
    let mode = std::env::args().nth(1).unwrap_or_else(|| "read".to_string());
    let e = keyring::Entry::new(SERVICE, ACCOUNT).expect("构造 Entry 失败");
    match mode.as_str() {
        "write" => match e.set_password("probe-value-from-A") {
            Ok(()) => println!("[A] write 成功"),
            Err(err) => println!("[A] write 失败: {err:?}"),
        },
        "read" => match e.get_password() {
            Ok(v) => println!("[A] read 成功: {v}"),
            Err(err) => println!("[A] read 失败: {err:?}"),
        },
        "delete" => match e.delete_credential() {
            Ok(()) => println!("[A] delete 成功"),
            Err(err) => println!("[A] delete 失败: {err:?}"),
        },
        other => println!("[A] 未知模式 {other}"),
    }
}
