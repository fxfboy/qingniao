//! 状态文件的跨进程写模型（D24 / CLI 方案 §7，M0b）。
//!
//! 覆盖 `quota.json` / `consumed.json` / `pending_deletes.json` 三个状态文件：
//! - **锁内重读**：写前取独占文件锁并重读磁盘最新值，调用方在锁内做合并
//!   （D24 ②：另一进程的写入不得被覆盖丢失）；
//! - **原子写**：pid 后缀唯一临时文件 → fsync → rename（D4；与 config.rs 同纪律）。
//!
//! 锁顺序（方案 §7 实现约束②）：**指纹锁 → 状态文件锁，单向**。
//! 本模块只提供状态文件锁；任何持状态文件锁的路径不得再取指纹锁。

use std::fs::{File, OpenOptions};
use std::io::{Read, Write};
use std::path::Path;
use std::time::{Duration, Instant};

const LOCK_TIMEOUT: Duration = Duration::from_secs(5);

/// 独占锁定状态文件（advisory），锁内重读磁盘内容并交调用方合并。
/// 返回 (锁 guard, 磁盘内容——不存在或非法时为 `None`)。
/// guard 存活期间锁有效；先落 guard 变量再使用内容，确保合并+写入全程持锁。
///
/// **inode 校验（为何锁上之后还要核对）**：[`atomic_write`] 用 rename 原子替换，
/// 会把路径指向**新 inode**。排队等待者若在他人持锁期间 `open` 了旧 inode，
/// 加锁成功后读到的是**陈旧内容**——跨进程的「锁内认领」会被重复执行
/// （cleanup 方案 U23：同 token 只删一次，被并发测试实测击穿）。
/// 因此锁上后核对 path 仍指向同一 inode，不一致则重开重试（限期内）。
pub fn with_exclusive(
    path: &Path,
) -> Result<(StateFileLock, Option<serde_json::Value>), String> {
    let deadline = Instant::now() + LOCK_TIMEOUT;
    loop {
        let mut f = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(path)
            .map_err(|e| format!("打开状态文件失败 {}: {e}", path.display()))?;
        match f.try_lock() {
            Ok(()) => {}
            Err(std::fs::TryLockError::WouldBlock) => {
                if Instant::now() >= deadline {
                    return Err(format!("状态文件被其他进程占用: {}", path.display()));
                }
                std::thread::sleep(Duration::from_millis(20));
                continue;
            }
            Err(std::fs::TryLockError::Error(e)) => {
                return Err(format!("状态文件加锁失败 {}: {e}", path.display()))
            }
        }
        if inode_of(&f) == inode_of_path(path) {
            let mut text = String::new();
            // 重读发生在锁内（D24：另一进程的写入必须可见）
            let _ = f.read_to_string(&mut text);
            let value = serde_json::from_str::<serde_json::Value>(&text).ok();
            return Ok((StateFileLock(f), value));
        }
        // 路径已被 rename 换成新 inode：释放旧锁，重开重试
        if Instant::now() >= deadline {
            return Err(format!("状态文件在锁等待期间被替换: {}", path.display()));
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// 文件句柄的 inode（非 unix 恒 0：校验退化为永真，保持既有行为）
fn inode_of(f: &File) -> u64 {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        f.metadata().map(|m| m.ino()).unwrap_or(0)
    }
    #[cfg(not(unix))]
    {
        let _ = f;
        0
    }
}

/// 路径当前指向的 inode（路径不存在时返回一个必不匹配的哨兵值）
fn inode_of_path(path: &Path) -> u64 {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        std::fs::metadata(path).map(|m| m.ino()).unwrap_or(u64::MAX)
    }
    #[cfg(not(unix))]
    {
        let _ = path;
        0
    }
}

/// 原子写：pid 后缀临时文件 → fsync → rename（0600，unix）。
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

/// 状态文件锁 guard（drop 即释放）
pub struct StateFileLock(#[allow(dead_code)] File);
