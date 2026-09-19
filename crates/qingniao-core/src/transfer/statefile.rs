//! 状态文件的跨进程写模型（D24 / CLI 方案 §7，M0b；D33 改为锁专用 .lock 文件）。
//!
//! 覆盖 `quota.json` / `consumed.json` / `pending_deletes.json` / `cleanup.json` 四个状态文件：
//! - **锁内重读**：写前取独占文件锁并重读磁盘最新值，调用方在锁内做合并
//!   （D24 ②：另一进程的写入不得被覆盖丢失）；
//! - **原子写**：pid 后缀唯一临时文件 → fsync → rename（D4；与 config.rs 同纪律）。
//!
//! **锁形状（D33）**：锁落在专用旁文件 `<name>.json.lock` 上（`File::create` +
//! advisory lock，与 config.rs `acquire_lock` 同款先例），**数据文件本身不再被锁**。
//! 为什么必须这样：`atomic_write` 用 rename 原子替换，会把路径指向新 inode/文件
//! 对象；若锁直接落在数据文件上，排队等待者早已 open 到旧句柄，锁上后读到的
//! 是**陈旧内容**——跨进程「锁内认领」会被重复执行（并发测试实测击穿 U23）。
//! std 打开文件默认带 `FILE_SHARE_DELETE`（Windows 下 rename 替换不被阻断，
//! `LockFileEx` 字节域锁拦不住），故该窗口**全平台同型**，不能靠平台差异豁免。
//! 锁文件从不被 rename，flock/LockFileEx 绑定稳定，互斥与读取新鲜值才同时成立。
//!
//! 锁顺序（方案 §7 实现约束②）：**指纹锁 → 状态文件锁，单向**。
//! 本模块只提供状态文件锁；任何持状态文件锁的路径不得再取指纹锁。

use std::fs::File;
use std::io::{Read, Write};
use std::path::Path;
use std::time::{Duration, Instant};

const LOCK_TIMEOUT: Duration = Duration::from_secs(5);

/// 独占锁定状态文件（advisory），锁内重读磁盘内容并交调用方合并。
/// 返回 (锁 guard, 磁盘内容——不存在或非法时为 `None`)。
/// guard 存活期间锁有效；先落 guard 变量再使用内容，确保合并+写入全程持锁。
///
/// 顺序保证：**先锁上锁文件，再打开数据文件读**——数据文件句柄读完即丢
/// （内容已在返回值里），guard 只持锁文件的 fd。锁文件从不被 rename，
/// 因此锁期间 rename 换数据文件不影响互斥，也不会读到锁前的陈旧内容。
pub fn with_exclusive(
    path: &Path,
) -> Result<(StateFileLock, Option<serde_json::Value>), String> {
    let lock_path = lock_file_of(path);
    let mut f = File::create(&lock_path)
        .map_err(|e| format!("创建状态文件锁失败 {}: {e}", lock_path.display()))?;
    acquire(&mut f, &lock_path)?;
    // 锁上之后才打开数据文件：此刻起磁盘内容不可能再被「持锁写」改动
    // （所有写路径都走本函数/atomic_write，且都在持锁期间）。
    let mut text = String::new();
    if let Ok(mut df) = File::open(path) {
        let _ = df.read_to_string(&mut text);
    }
    let value = serde_json::from_str::<serde_json::Value>(&text).ok();
    Ok((StateFileLock(f), value))
}

/// 状态文件的锁专用旁文件：`quota.json` → `quota.json.lock`。
/// 四个状态文件都是 `*.json`，`with_extension("json.lock")` 恰好得到该形状
/// （与 config.rs `acquire_lock` 同款）。
fn lock_file_of(path: &Path) -> std::path::PathBuf {
    path.with_extension("json.lock")
}

fn acquire(f: &mut File, lock_path: &Path) -> Result<(), String> {
    let deadline = Instant::now() + LOCK_TIMEOUT;
    loop {
        match f.try_lock() {
            Ok(()) => return Ok(()),
            Err(std::fs::TryLockError::WouldBlock) => {}
            Err(std::fs::TryLockError::Error(e)) => {
                return Err(format!("状态文件加锁失败 {}: {e}", lock_path.display()))
            }
        }
        if Instant::now() >= deadline {
            return Err(format!("状态文件被其他进程占用: {}", lock_path.display()));
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// 原子写：pid 后缀临时文件 → fsync → rename（0600，unix）。
///
/// **不变量（D33 评审 P2-1，调用方必须遵守）**：
/// 1. 只能由**当前持有 `path` 对应状态文件锁**（[`with_exclusive`] 返回的 guard
///    存活期间）的调用方调用；
/// 2. **rename 必须是临界区最后一个动作**——rename 完成的瞬间新内容即处于
///    无锁状态，任何等待者随后锁上锁文件、`File::open(path)` 读到的都是新值；
///    rename 之后不得再在锁内做任何与该文件相关的读改写。
pub fn atomic_write(path: &Path, data: &serde_json::Value) -> Result<(), String> {
    let json = serde_json::to_string(data).map_err(|e| format!("状态文件序列化失败: {e}"))?;
    let ext = path
        .extension()
        .map(|e| format!("{}.tmp.{}", e.to_string_lossy(), std::process::id()))
        .unwrap_or_else(|| format!("tmp.{}", std::process::id()));
    let tmp = path.with_extension(ext);
    {
        let mut f = File::create(&tmp).map_err(|e| format!("写状态临时文件失败: {e}"))?;
        f.write_all(json.as_bytes()).map_err(|e| format!("写状态临时文件失败: {e}"))?;
        f.sync_all().map_err(|e| format!("状态临时文件刷盘失败: {e}"))?;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o600));
    }
    std::fs::rename(&tmp, path).map_err(|e| {
        let _ = std::fs::remove_file(&tmp);
        format!("状态文件原子替换失败: {e}")
    })
}

/// 便利封装：锁内重读 → 调用方合并出**完整新值** → 原子写。
/// `merge` 收到磁盘现有值（`None` = 文件不存在/不可解析），返回要落盘的完整新值。
pub fn update<T, F>(path: &Path, merge: F) -> Result<(), String>
where
    T: serde::Serialize,
    F: FnOnce(Option<&serde_json::Value>) -> Result<T, String>,
{
    let (guard, disk) = with_exclusive(path)?;
    let new_value = merge(disk.as_ref())?;
    let v = serde_json::to_value(&new_value).map_err(|e| format!("状态序列化失败: {e}"))?;
    atomic_write(path, &v)?;
    drop(guard);
    Ok(())
}

/// 状态文件锁 guard（drop 即释放；持有的是**锁文件**的 fd，数据文件句柄不跨函数存活）
pub struct StateFileLock(#[allow(dead_code)] File);

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;
    use std::time::{Duration, Instant};

    fn temp_dir(tag: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("qn-statefile-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// D33 互斥断言：T1 持锁 → 读旧值 → 持锁期间 atomic_write 写新值 → 释放；
    /// T2 在 T1 持锁期间调 with_exclusive，必须阻塞到 T1 释放，且**必须读到新值**。
    /// （旧「锁数据文件 + rename」形状下，T2 会锁到被 rename 换掉的旧句柄、读到陈旧值。）
    #[test]
    fn exclusive_lock_blocks_and_reader_sees_fresh_value() {
        let dir = temp_dir("mutex");
        let path = dir.join("quota.json");
        atomic_write(&path, &serde_json::json!({ "v": 1 })).unwrap();

        let t1_path = path.clone();
        let entered = Arc::new(AtomicUsize::new(0));
        let entered2 = entered.clone();
        let t1 = std::thread::spawn(move || {
            let (guard, disk) = with_exclusive(&t1_path).unwrap();
            assert_eq!(disk.unwrap()["v"], 1, "T1 应读到旧值");
            entered2.store(1, Ordering::SeqCst);
            std::thread::sleep(Duration::from_millis(200));
            // T1 持锁期间写新值（rename = 临界区最后一个动作）
            atomic_write(&t1_path, &serde_json::json!({ "v": 2 })).unwrap();
            drop(guard);
        });
        let t2 = std::thread::spawn(move || {
            while entered.load(Ordering::SeqCst) == 0 {
                std::hint::spin_loop();
            }
            let t0 = Instant::now();
            let (_guard, disk) = with_exclusive(&path).unwrap();
            assert!(
                t0.elapsed() >= Duration::from_millis(150),
                "T2 必须阻塞到 T1 释放（实际等待 {:?}）",
                t0.elapsed()
            );
            assert_eq!(
                disk.unwrap()["v"],
                2,
                "T2 必须读到 T1 持锁期间写入的新值（不得读陈旧内容）"
            );
        });
        t1.join().unwrap();
        t2.join().unwrap();
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 锁文件形状：`*.json` → `*.json.lock`（四个状态文件共用该约定）
    #[test]
    fn lock_file_naming() {
        let p = std::path::Path::new("/x/y/quota.json");
        assert_eq!(lock_file_of(p), std::path::Path::new("/x/y/quota.json.lock"));
        let p = std::path::Path::new("/x/y/cleanup.json");
        assert_eq!(lock_file_of(p), std::path::Path::new("/x/y/cleanup.json.lock"));
    }
}
