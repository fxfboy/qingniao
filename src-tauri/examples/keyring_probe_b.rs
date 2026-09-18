//! M0a 出口 5 探针（B 侧二进制）：与 A 是**不同的 unsigned 二进制**，用来实测
//! 「A 写入的 keychain 条目，B 能否读到」——即 APP 写、CLI 读的真实场景。
//!
//! 安全约束同 A：只用独立探针条目，不触碰 `com.qingniao.transfer` 下的真实主密钥。

const SERVICE: &str = "com.qingniao.transfer.PROBE";
const ACCOUNT: &str = "probe-key-A"; // 故意与 A 相同：模拟「B 读 A 写的条目」

fn main() {
    let mode = std::env::args().nth(1).unwrap_or_else(|| "read".to_string());
    let e = keyring::Entry::new(SERVICE, ACCOUNT).expect("构造 Entry 失败");
    match mode.as_str() {
        "read" => match e.get_password() {
            Ok(v) => println!("[B] read 成功: {v}"),
            Err(err) => println!("[B] read 失败: {err:?}"),
        },
        "overwrite" => match e.set_password("probe-value-overwritten-by-B") {
            Ok(()) => println!("[B] overwrite 成功"),
            Err(err) => println!("[B] overwrite 失败: {err:?}"),
        },
        other => println!("[B] 未知模式 {other}"),
    }
}
