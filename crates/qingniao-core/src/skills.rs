//! Agent 集成安装（方案 v3 §七 D13，完全模仿 paseo 的 integrations 模块）。
//!
//! 技能：随应用 bundle 的 `skills/<name>/` 托管同步到固定目录
//! （`~/.agents/skills`、`~/.claude/skills`、`~/.codex/skills`），目录清单单一常量可扩展；
//! 托管清单 `.qingniao-managed-files.json` 只管理自己写入的文件，不碰用户其他文件；
//! 逐文件 sha256 比对出三态状态机：not-installed / up-to-date / drift。
//!
//! CLI：二进制安装（unix 符号链接 / Windows 复制）+ shell rc PATH 追加（幂等）。

use serde::Serialize;
use sha2::Digest;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

pub const MANAGED_MANIFEST: &str = ".qingniao-managed-files.json";
pub const SKILL_NAME: &str = "qingniao";

/// 技能安装目标（paseo 同款三目录；扩展只加一行）
pub struct SkillTarget {
    pub label: &'static str,
    pub dir: PathBuf,
}

pub fn default_targets(home: &Path) -> Vec<SkillTarget> {
    [".agents", ".claude", ".codex"]
        .iter()
        .map(|d| SkillTarget {
            label: match *d {
                ".agents" => "agents",
                ".claude" => "claude",
                _ => "codex",
            },
            dir: home.join(d).join("skills"),
        })
        .collect()
}

// ===== 哈希 =====

type FileHashes = BTreeMap<String, String>;

fn sha256_hex(bytes: &[u8]) -> String {
    let mut h = sha2::Sha256::new();
    h.update(bytes);
    format!("{:x}", h.finalize())
}

/// 目录内逐文件 sha256（rel posix 路径 → hex）；目录不存在 → None。跳过托管清单自身。
fn hash_skill_dir(root: &Path) -> std::io::Result<Option<FileHashes>> {
    if !root.is_dir() {
        return Ok(None);
    }
    let mut out = FileHashes::new();
    walk(root, root, &mut out)?;
    Ok(Some(out))
}

fn walk(base: &Path, dir: &Path, out: &mut FileHashes) -> std::io::Result<()> {
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();
        if path.is_dir() {
            walk(base, &path, out)?;
        } else if path.is_file() {
            let rel = path
                .strip_prefix(base)
                .expect("walk 以 base 为根")
                .to_string_lossy()
                .replace('\\', "/");
            if rel == MANAGED_MANIFEST {
                continue;
            }
            let bytes = std::fs::read(&path)?;
            out.insert(rel, sha256_hex(&bytes));
        }
    }
    Ok(())
}

fn bundle_hashes(source_dir: &Path) -> Result<FileHashes, String> {
    let skill_src = source_dir.join(SKILL_NAME);
    hash_skill_dir(&skill_src)
        .map_err(|e| format!("读取技能源失败 {}: {e}", skill_src.display()))?
        .ok_or_else(|| format!("技能源缺失: {}（开发模式请确认仓库内 skills/qingniao/ 存在）", skill_src.display()))
}

// ===== 状态机 =====

#[derive(Debug, Clone, Serialize)]
pub struct TargetStatus {
    pub label: String,
    pub dir: String,
    pub installed: bool,
    pub up_to_date: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct SkillsStatus {
    /// not-installed | up-to-date | drift | source-missing
    pub state: String,
    /// 技能源（bundle 内 `skills/qingniao/`）是否可用；false 时 state 恒为 source-missing
    pub source_available: bool,
    pub targets: Vec<TargetStatus>,
}

/// 技能源缺失（Windows 便携包等未内置 `skills/` 的场景）：
/// 不返回 Err——否则 UI 无法区分「未打包」与「未安装」，只能停在「检测中…」。
/// 各目标仍按磁盘实况报告 installed，卸载按钮据此可用。
pub fn missing_source_status(targets: &[SkillTarget]) -> SkillsStatus {
    let tstats = targets
        .iter()
        .map(|t| {
            let skill_dir = t.dir.join(SKILL_NAME);
            let installed = hash_skill_dir(&skill_dir)
                .ok()
                .flatten()
                .map(|h| !h.is_empty())
                .unwrap_or(false);
            TargetStatus {
                label: t.label.to_string(),
                dir: t.dir.to_string_lossy().to_string(),
                installed,
                up_to_date: false,
            }
        })
        .collect();
    SkillsStatus {
        state: "source-missing".to_string(),
        source_available: false,
        targets: tstats,
    }
}

/// 三态判定（paseo 同语义）：任一目标未安装/不一致 → drift；全装且全一致 → up-to-date
pub fn get_status(source_dir: &Path, targets: &[SkillTarget]) -> Result<SkillsStatus, String> {
    let bundle = bundle_hashes(source_dir)?;
    let mut tstats = Vec::new();
    for t in targets {
        let skill_dir = t.dir.join(SKILL_NAME);
        let disk = hash_skill_dir(&skill_dir)
            .map_err(|e| format!("读取 {} 失败: {e}", skill_dir.display()))?;
        let (installed, up_to_date) = match &disk {
            None => (false, false),
            Some(h) => {
                let matches = bundle.iter().all(|(k, v)| h.get(k) == Some(v));
                (true, matches)
            }
        };
        tstats.push(TargetStatus {
            label: t.label.to_string(),
            dir: t.dir.to_string_lossy().to_string(),
            installed,
            up_to_date,
        });
    }
    let any_installed = tstats.iter().any(|t| t.installed);
    let all_installed_uptodate = tstats.iter().all(|t| t.installed && t.up_to_date);
    let state = if !any_installed {
        "not-installed"
    } else if all_installed_uptodate {
        "up-to-date"
    } else {
        "drift"
    };
    Ok(SkillsStatus {
        state: state.to_string(),
        source_available: true,
        targets: tstats,
    })
}

// ===== 托管清单 =====

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
struct ManagedManifest {
    version: u32,
    files: FileHashes,
}

fn read_manifest(skill_dir: &Path) -> Result<Option<ManagedManifest>, String> {
    let path = skill_dir.join(MANAGED_MANIFEST);
    if !path.exists() {
        return Ok(None);
    }
    let raw = std::fs::read_to_string(&path).map_err(|e| format!("读取托管清单失败: {e}"))?;
    match serde_json::from_str::<ManagedManifest>(&raw) {
        Ok(m) if m.version == 1 => Ok(Some(m)),
        _ => Ok(None), // 损坏/未知版本：不管理、不删除
    }
}

fn write_manifest(skill_dir: &Path, files: &FileHashes) -> Result<(), String> {
    let manifest = ManagedManifest {
        version: 1,
        files: files.clone(),
    };
    let data = serde_json::to_string_pretty(&manifest).map_err(|e| e.to_string())?;
    std::fs::write(skill_dir.join(MANAGED_MANIFEST), data).map_err(|e| format!("写托管清单失败: {e}"))
}

// ===== 安装 / 升级 / 卸载 =====

/// 把 bundle 同步到全部目标目录（install 与 update 同实现，幂等覆盖）
pub fn install(source_dir: &Path, targets: &[SkillTarget]) -> Result<(), String> {
    let bundle = bundle_hashes(source_dir)?;
    let skill_src = source_dir.join(SKILL_NAME);
    for t in targets {
        let skill_dir = t.dir.join(SKILL_NAME);
        std::fs::create_dir_all(&skill_dir)
            .map_err(|e| format!("创建目录失败 {}: {e}", skill_dir.display()))?;
        // 旧清单里已不在 bundle 的文件 → 删除（升级时清理历史托管文件）
        let old = read_manifest(&skill_dir)?;
        if let Some(old) = &old {
            for rel in old.files.keys() {
                if !bundle.contains_key(rel) {
                    let _ = std::fs::remove_file(skill_dir.join(rel));
                }
            }
        }
        let mut managed = FileHashes::new();
        for (rel, hash) in &bundle {
            let src = skill_src.join(rel);
            let dst = skill_dir.join(rel);
            let need_copy = match std::fs::read(&dst) {
                Ok(dst_bytes) => sha256_hex(&dst_bytes) != *hash,
                Err(_) => true,
            };
            if need_copy {
                if let Some(parent) = dst.parent() {
                    std::fs::create_dir_all(parent)
                        .map_err(|e| format!("创建目录失败 {}: {e}", parent.display()))?;
                }
                std::fs::copy(&src, &dst)
                    .map_err(|e| format!("写入 {} 失败: {e}", dst.display()))?;
            }
            managed.insert(rel.clone(), hash.clone());
        }
        write_manifest(&skill_dir, &managed)?;
    }
    Ok(())
}

/// 卸载：只删托管清单内的文件（npx skills add 等用户自装内容不受影响）；
/// 随后自底向上清理空目录
pub fn uninstall(targets: &[SkillTarget]) -> Result<(), String> {
    for t in targets {
        let skill_dir = t.dir.join(SKILL_NAME);
        let Some(manifest) = read_manifest(&skill_dir)? else {
            continue;
        };
        for rel in manifest.files.keys() {
            let _ = std::fs::remove_file(skill_dir.join(rel));
        }
        let _ = std::fs::remove_file(skill_dir.join(MANAGED_MANIFEST));
        remove_empty_dirs_bottom_up(&skill_dir);
    }
    Ok(())
}

/// 自底向上尝试删除空目录（非空时静默保留，用户残留文件安全）
fn remove_empty_dirs_bottom_up(root: &Path) {
    fn collect(dir: &Path, stack: &mut Vec<PathBuf>) {
        if let Ok(entries) = std::fs::read_dir(dir) {
            for e in entries.flatten() {
                if e.path().is_dir() {
                    collect(&e.path(), stack);
                }
            }
        }
        stack.push(dir.to_path_buf());
    }
    // collect 为后序（子目录先于父目录入栈），按原序删除即自底向上
    let mut stack: Vec<PathBuf> = Vec::new();
    collect(root, &mut stack);
    for dir in stack {
        let _ = std::fs::remove_dir(dir); // 非空会失败，静默保留用户残留
    }
}

/// drift 自动更新：状态为 drift 时重新同步（应用启动时调用）
pub fn auto_update(source_dir: &Path, targets: &[SkillTarget]) -> Result<bool, String> {
    let status = get_status(source_dir, targets)?;
    if status.state != "drift" {
        return Ok(false);
    }
    install(source_dir, targets)?;
    Ok(true)
}

// ===== CLI 二进制安装 =====

/// unix 符号链接（跟随应用升级）/ Windows 复制（D13）
pub fn install_cli_binary(source: &Path, target: &Path) -> Result<(), String> {
    if !source.exists() {
        return Err(format!("CLI 二进制不存在: {}", source.display()));
    }
    if let Some(parent) = target.parent() {
        std::fs::create_dir_all(parent).map_err(|e| format!("创建目录失败: {e}"))?;
    }
    if target.symlink_metadata().is_ok() {
        std::fs::remove_file(target).map_err(|e| format!("清理旧安装失败: {e}"))?;
    }
    #[cfg(unix)]
    {
        std::os::unix::fs::symlink(source, target)
            .map_err(|e| format!("创建符号链接失败: {e}"))?;
    }
    #[cfg(windows)]
    {
        std::fs::copy(source, target).map_err(|e| format!("复制 CLI 失败: {e}"))?;
    }
    Ok(())
}

pub fn cli_target_path(home: &Path) -> PathBuf {
    if cfg!(target_os = "windows") {
        // Windows 走 %LOCALAPPDATA%qingniao\bin（由调用方传入等效 home 不可行），
        // 这里以 home 相对约定不便；实际路径由 APP 侧用 LOCALAPPDATA 解析后传 target。
        let _ = home;
        unreachable!("Windows 目标路径由调用方显式传入")
    } else {
        home.join(".local").join("bin").join("qingniao")
    }
}

/// 幂等往 shell rc 追加 PATH（仅 bash/zsh；返回是否修改）。`shell` 取 $SHELL 文件名。
pub fn ensure_path_in_shell_rc(home: &Path, shell: &str, bin_dir: &Path) -> Result<bool, String> {
    let rc = match shell.rsplit('/').next().unwrap_or("") {
        "zsh" => home.join(".zshrc"),
        "bash" => home.join(".bashrc"),
        "" => home.join(".profile"),
        _ => return Ok(false), // fish 等不处理
    };
    let line = format!("export PATH=\"{}:$PATH\"", bin_dir.display());
    let existing = std::fs::read_to_string(&rc).unwrap_or_default();
    if existing.lines().any(|l| l.contains(bin_dir.to_string_lossy().as_ref())) {
        return Ok(false);
    }
    let mut next = existing;
    if !next.is_empty() && !next.ends_with('\n') {
        next.push('\n');
    }
    next.push_str(&format!("\n# added by qingniao: expose qingniao CLI\n{line}\n"));
    std::fs::write(&rc, next).map_err(|e| format!("写入 {} 失败: {e}", rc.display()))?;
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(p: &Path, s: &str) {
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, s).unwrap();
    }

    fn source_fixture(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("qn-skills-src-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        write(&dir.join(SKILL_NAME).join("SKILL.md"), "v1\n");
        write(&dir.join(SKILL_NAME).join("refs").join("api.md"), "api\n");
        dir
    }

    #[test]
    fn install_status_uninstall_lifecycle() {
        let src = source_fixture("lifecycle");
        let home = std::env::temp_dir().join(format!("qn-skills-home-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&home);
        let targets = default_targets(&home);

        let st = get_status(&src, &targets).unwrap();
        assert_eq!(st.state, "not-installed");

        install(&src, &targets).unwrap();
        let st = get_status(&src, &targets).unwrap();
        assert_eq!(st.state, "up-to-date");
        // 三个目录 + 清单存在
        for t in &targets {
            assert!(t.dir.join(SKILL_NAME).join("SKILL.md").exists());
            assert!(t.dir.join(SKILL_NAME).join(MANAGED_MANIFEST).exists());
        }

        uninstall(&targets).unwrap();
        for t in &targets {
            let d = t.dir.join(SKILL_NAME);
            eprintln!("DBG {} exists={}", d.display(), d.exists());

        }
        let st = get_status(&src, &targets).unwrap();
        assert_eq!(st.state, "not-installed");
    }

    /// 源缺失（安装包没带 skills/）时不得返回 Err——否则 UI 只能停在「检测中…」；
    /// 各目标仍按磁盘实况报告 installed，卸载按钮据此可用
    #[test]
    fn missing_source_reports_installed_truthfully() {
        let home = std::env::temp_dir().join(format!("qn-skills-nosrc-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&home);
        let targets = default_targets(&home);

        let st = missing_source_status(&targets);
        assert_eq!(st.state, "source-missing");
        assert!(!st.source_available);
        assert!(st.targets.iter().all(|t| !t.installed));

        // 用户此前装过技能：源缺失也应如实报 installed（否则卸载入口会消失）
        write(&targets[0].dir.join(SKILL_NAME).join("SKILL.md"), "old\n");
        let st = missing_source_status(&targets);
        assert!(st.targets[0].installed);
        assert!(!st.targets[1].installed);

        // 空目录不算已安装
        std::fs::create_dir_all(targets[2].dir.join(SKILL_NAME)).unwrap();
        let st = missing_source_status(&targets);
        assert!(!st.targets[2].installed);
    }

    /// 正常路径必须显式声明源可用，前端据此区分「未安装」与「未打包」
    #[test]
    fn get_status_marks_source_available() {
        let src = source_fixture("srcavail");
        let home = std::env::temp_dir().join(format!("qn-skills-srcavail-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&home);
        let st = get_status(&src, &default_targets(&home)).unwrap();
        assert!(st.source_available);
    }

    #[test]
    fn drift_detected_and_auto_update_restores() {
        let src = source_fixture("drift");
        let home = std::env::temp_dir().join(format!("qn-skills-drift-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&home);
        let targets = default_targets(&home);
        install(&src, &targets).unwrap();

        // 用户改了托管文件 → drift
        write(&targets[0].dir.join(SKILL_NAME).join("SKILL.md"), "tampered\n");
        let st = get_status(&src, &targets).unwrap();
        assert_eq!(st.state, "drift");

        // 升级 bundle（加一个文件）后漂移仍在；auto_update 恢复
        write(&src.join(SKILL_NAME).join("extra.md"), "new\n");
        let st = get_status(&src, &targets).unwrap();
        assert_eq!(st.state, "drift");
        assert!(auto_update(&src, &targets).unwrap());
        let st = get_status(&src, &targets).unwrap();
        assert_eq!(st.state, "up-to-date");
        assert!(targets[0].dir.join(SKILL_NAME).join("extra.md").exists());
        // 幂等：不再漂移时 auto_update 不动作
        assert!(!auto_update(&src, &targets).unwrap());
    }

    #[test]
    fn uninstall_touches_only_managed_files() {
        let src = source_fixture("managed");
        let home = std::env::temp_dir().join(format!("qn-skills-managed-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&home);
        let targets = default_targets(&home);
        install(&src, &targets).unwrap();
        // 用户自加文件
        let user_file = targets[1].dir.join(SKILL_NAME).join("my-notes.md");
        write(&user_file, "mine\n");
        uninstall(&targets).unwrap();
        assert!(!targets[1].dir.join(SKILL_NAME).join("SKILL.md").exists());
        assert!(user_file.exists(), "用户自有文件必须保留");
    }

    #[test]
    fn stale_managed_file_removed_on_update() {
        let src = source_fixture("stale");
        let home = std::env::temp_dir().join(format!("qn-skills-stale-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&home);
        let targets = default_targets(&home);
        install(&src, &targets).unwrap();
        assert!(targets[0].dir.join(SKILL_NAME).join("refs").join("api.md").exists());
        // bundle 移除 refs/api.md 后升级
        std::fs::remove_file(src.join(SKILL_NAME).join("refs").join("api.md")).unwrap();
        install(&src, &targets).unwrap();
        assert!(!targets[0].dir.join(SKILL_NAME).join("refs").join("api.md").exists());
    }

    #[cfg(unix)]
    #[test]
    fn cli_install_symlink_and_path_rc() {
        let home = std::env::temp_dir().join(format!("qn-skills-cli-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&home);
        let fake_bin = home.join("app").join("qingniao-cli");
        write(&fake_bin, "#!/bin/sh\nexit 0\n");

        let target = home.join(".local").join("bin").join("qingniao");
        install_cli_binary(&fake_bin, &target).unwrap();
        assert!(target.exists());
        assert_eq!(
            std::fs::read_link(&target).unwrap(),
            fake_bin,
            "unix 上必须是符号链接（跟随应用升级）"
        );

        // PATH：zshrc 追加一次，二次幂等
        assert!(ensure_path_in_shell_rc(&home, "/bin/zsh", &home.join(".local/bin")).unwrap());
        assert!(!ensure_path_in_shell_rc(&home, "/bin/zsh", &home.join(".local/bin")).unwrap());
        let rc = std::fs::read_to_string(home.join(".zshrc")).unwrap();
        assert!(rc.contains(".local/bin"));
        // 不支持的 shell 不动
        assert!(!ensure_path_in_shell_rc(&home, "/usr/bin/fish", &home.join(".local/bin")).unwrap());
    }
}
