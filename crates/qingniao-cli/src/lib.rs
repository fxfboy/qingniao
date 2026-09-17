//! 青鸟 CLI（bin: qingniao-cli，安装时复制为 qingniao）。方案 v3 §六。
//!
//! 契约：
//! - `--json` 时 stdout 只输出一个机器可读 JSON 对象，诊断信息全部走 stderr
//! - 退出码：0 成功（含 dry-run）；1 用法/配置错误；2 发送失败（细分在 error.kind）
//! - secret 只从 stdin / 环境变量读（D11）；URL 默认白名单，`--allow-insecure-url` 放开（D10）
//!
//! 命令逻辑放在 lib（`cmd_*` 函数，显式传 `config_path`）以便集成测试直接驱动。

use clap::{Args, Parser, Subcommand, ValueEnum};
use qingniao_core::config::{
    check_url_policy, load, modify, resolve_config_path, Config, LoadedConfig,
};
use qingniao_core::message::{build_payload, detect_type, MsgType};
use qingniao_core::send::{
    dispatch_send, record_send, upload_image, RecordedSend, SendErrorKind, SendFailure,
};
use serde_json::{json, Value};
use std::io::Read;
use std::path::{Path, PathBuf};

const MAX_IMAGE_BYTES: u64 = 10 * 1024 * 1024;

// ===== 错误与输出 =====

#[derive(Debug, Clone)]
pub struct CliFail {
    pub kind: SendErrorKind,
    pub message: String,
}

impl CliFail {
    fn usage(message: impl Into<String>) -> Self {
        CliFail { kind: SendErrorKind::Usage, message: message.into() }
    }
    fn config(message: impl Into<String>) -> Self {
        CliFail { kind: SendErrorKind::Config, message: message.into() }
    }
    fn from_send(f: SendFailure) -> Self {
        CliFail { kind: f.kind, message: f.message }
    }
    pub fn exit_code(&self) -> i32 {
        match self.kind {
            SendErrorKind::Usage | SendErrorKind::Config => 1,
            _ => 2,
        }
    }
}

#[derive(Debug)]
pub enum Out {
    Json(Value),
    Text(String),
}

#[derive(Debug)]
pub struct Outcome {
    pub out: Out,
    pub code: i32,
}

impl Outcome {
    fn ok_json(v: Value) -> Self {
        Outcome { out: Out::Json(v), code: 0 }
    }
    fn ok_text(s: impl Into<String>) -> Self {
        Outcome { out: Out::Text(s.into()), code: 0 }
    }
}

fn emit(out: Out) {
    match out {
        Out::Json(v) => println!("{}", serde_json::to_string_pretty(&v).expect("JSON 序列化不可失败")),
        Out::Text(s) => println!("{s}"),
    }
}

fn warn_stderr(msg: &str) {
    eprintln!("[青鸟] 警告: {msg}");
}

fn print_warnings(warnings: &[String]) {
    for w in warnings {
        warn_stderr(w);
    }
}

// ===== CLI 定义 =====

#[derive(Parser)]
#[command(name = "qingniao", version, about = "青鸟 · 飞书消息助手 CLI — 向飞书自定义机器人发送消息")]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// 发送消息（自动识别类型）
    Send(SendArgs),
    /// 机器人管理
    Bot {
        #[command(subcommand)]
        action: BotCmd,
    },
    /// 上传图片换 image_key
    UploadImage {
        /// 图片文件路径
        #[arg(long)]
        file: PathBuf,
        #[arg(long)]
        json: bool,
    },
    /// 配置路径 / 脱敏概览
    Config {
        #[command(subcommand)]
        action: ConfigCmd,
    },
    /// 发送记录（与 APP 互通）
    History {
        #[arg(long, default_value_t = 20)]
        limit: usize,
        #[arg(long)]
        json: bool,
    },
    /// 环境自检（无副作用）
    Doctor {
        #[arg(long)]
        json: bool,
    },
}

#[derive(Args)]
pub struct SendArgs {
    /// 消息内容（与 --stdin/--file 三选一）
    pub text: Option<String>,
    /// 强制消息类型；缺省 auto 自动识别（wire type）
    #[arg(short, long)]
    pub r#type: Option<TypeArg>,
    /// 目标机器人（id > 名称 > 下标，下标从 1 开始）；缺省用默认机器人
    #[arg(short, long)]
    pub bot: Option<String>,
    /// 上传本地图片并附加（可重复；image 类型只取第一张，与 APP 基线一致）
    #[arg(long)]
    pub image: Vec<PathBuf>,
    /// 已有 image_key（可重复）
    #[arg(long = "image-key")]
    pub image_key: Vec<String>,
    /// 从 stdin 读内容（多行/卡片 JSON/超长内容）
    #[arg(long)]
    pub stdin: bool,
    /// 从文件读内容
    #[arg(long)]
    pub file: Option<PathBuf>,
    /// post 标题覆盖
    #[arg(long)]
    pub title: Option<String>,
    /// 只组装 payload 预览，不发送、不写历史
    #[arg(long)]
    pub dry_run: bool,
    /// dry-run 时输出真实 timestamp+sign（默认 "omitted"，避免认证材料进 stdout）
    #[arg(long)]
    pub show_sign: bool,
    /// 放开 URL 白名单（内网 mock / 自建网关）
    #[arg(long = "allow-insecure-url")]
    pub allow_insecure_url: bool,
    #[arg(long)]
    pub json: bool,
}

#[derive(Clone, Copy, ValueEnum)]
pub enum TypeArg {
    Auto,
    Text,
    Post,
    Image,
    Interactive,
}

impl TypeArg {
    fn wire(&self) -> Option<MsgType> {
        match self {
            TypeArg::Auto => None,
            TypeArg::Text => Some(MsgType::Text),
            TypeArg::Post => Some(MsgType::Post),
            TypeArg::Image => Some(MsgType::Image),
            TypeArg::Interactive => Some(MsgType::Interactive),
        }
    }
}

#[derive(Subcommand)]
enum BotCmd {
    /// 列出机器人
    Ls {
        #[arg(long)]
        json: bool,
    },
    /// 添加机器人（secret 走 --secret-stdin 或环境变量 QINGNIAO_BOT_SECRET，不接受命令行参数）
    Add {
        #[arg(long)]
        name: String,
        #[arg(long)]
        url: String,
        #[arg(long = "secret-stdin")]
        secret_stdin: bool,
        #[arg(long = "allow-insecure-url")]
        allow_insecure_url: bool,
        #[arg(long)]
        json: bool,
    },
    /// 删除机器人
    Rm {
        key: String,
        #[arg(long)]
        json: bool,
    },
    /// 设为默认机器人
    Use {
        key: String,
        #[arg(long)]
        json: bool,
    },
}

#[derive(Subcommand)]
enum ConfigCmd {
    /// 打印配置文件路径
    Path {
        #[arg(long)]
        json: bool,
    },
    /// 脱敏概览
    Show {
        #[arg(long)]
        json: bool,
    },
}

// ===== 入口 =====

pub fn run() -> i32 {
    let cli = Cli::parse();
    let result = (|| -> Result<Outcome, CliFail> {
        match cli.cmd {
            Cmd::Send(args) => {
                let path = resolve_config_path().map_err(CliFail::config)?;
                cmd_send(&path, args)
            }
            Cmd::Bot { action } => match action {
                BotCmd::Ls { json } => {
                    let path = resolve_config_path().map_err(CliFail::config)?;
                    cmd_bot_ls(&path, json)
                }
                BotCmd::Add { name, url, secret_stdin, allow_insecure_url, json } => {
                    let path = resolve_config_path().map_err(CliFail::config)?;
                    cmd_bot_add(&path, &name, &url, secret_stdin, allow_insecure_url, json)
                }
                BotCmd::Rm { key, json } => {
                    let path = resolve_config_path().map_err(CliFail::config)?;
                    cmd_bot_rm(&path, &key, json)
                }
                BotCmd::Use { key, json } => {
                    let path = resolve_config_path().map_err(CliFail::config)?;
                    cmd_bot_use(&path, &key, json)
                }
            },
            Cmd::UploadImage { file, json } => {
                let path = resolve_config_path().map_err(CliFail::config)?;
                cmd_upload_image(&path, &file, json)
            }
            Cmd::Config { action } => match action {
                ConfigCmd::Path { json } => {
                    let path = resolve_config_path().map_err(CliFail::config)?;
                    Ok(if json {
                        Outcome::ok_json(json!({ "path": path.display().to_string() }))
                    } else {
                        Outcome::ok_text(path.display().to_string())
                    })
                }
                ConfigCmd::Show { json } => {
                    let path = resolve_config_path().map_err(CliFail::config)?;
                    cmd_config_show(&path, json)
                }
            },
            Cmd::History { limit, json } => {
                let path = resolve_config_path().map_err(CliFail::config)?;
                cmd_history(&path, limit, json)
            }
            Cmd::Doctor { json } => {
                let path = resolve_config_path().map_err(CliFail::config)?;
                cmd_doctor(&path, json)
            }
        }
    })();
    match result {
        Ok(outcome) => {
            emit(outcome.out);
            outcome.code
        }
        Err(f) => {
            eprintln!("[青鸟] 错误({}): {}", kind_name(f.kind), f.message);
            f.exit_code()
        }
    }
}

fn kind_name(k: SendErrorKind) -> &'static str {
    match k {
        SendErrorKind::Usage => "usage",
        SendErrorKind::Config => "config",
        SendErrorKind::Network => "network",
        SendErrorKind::Timeout => "timeout",
        SendErrorKind::Http => "http",
        SendErrorKind::Feishu => "feishu",
    }
}

// ===== 配置读取辅助 =====

fn load_or_fail(config_path: &Path) -> Result<LoadedConfig, CliFail> {
    load(config_path).map_err(CliFail::config)
}

fn app_id_of(cfg: &Config) -> String {
    cfg.raw.get("app_id").and_then(|v| v.as_str()).unwrap_or("").to_string()
}
fn app_secret_of(cfg: &Config) -> String {
    cfg.raw.get("app_secret").and_then(|v| v.as_str()).unwrap_or("").to_string()
}

fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn read_secret_from_stdin() -> Result<String, CliFail> {
    let mut buf = String::new();
    std::io::stdin()
        .read_line(&mut buf)
        .map_err(|e| CliFail::usage(format!("读取 secret 失败: {e}")))?;
    Ok(buf.trim().to_string())
}

fn upload_local_image(cfg: &Config, path: &Path) -> Result<String, CliFail> {
    let meta = std::fs::metadata(path)
        .map_err(|e| CliFail::usage(format!("图片文件不可读 {}: {e}", path.display())))?;
    if meta.len() > MAX_IMAGE_BYTES {
        return Err(CliFail::usage(format!(
            "图片超过 10MB 上限: {}（{} 字节）",
            path.display(),
            meta.len()
        )));
    }
    let bytes = std::fs::read(path)
        .map_err(|e| CliFail::usage(format!("图片读取失败 {}: {e}", path.display())))?;
    let filename = path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("image.png")
        .to_string();
    upload_image(&app_id_of(cfg), &app_secret_of(cfg), bytes, &filename).map_err(CliFail::from_send)
}

// ===== send =====

pub fn cmd_send(config_path: &Path, args: SendArgs) -> Result<Outcome, CliFail> {
    let loaded = load_or_fail(config_path)?;
    print_warnings(&loaded.warnings);
    let cfg = loaded.config;

    // 内容来源：positional / stdin / file 三选一
    let text: Option<String> = if args.stdin {
        if args.text.is_some() || args.file.is_some() {
            return Err(CliFail::usage("--stdin 不能与位置参数或 --file 同时使用"));
        }
        let mut buf = String::new();
        std::io::stdin()
            .read_to_string(&mut buf)
            .map_err(|e| CliFail::usage(format!("读取 stdin 失败: {e}")))?;
        Some(buf)
    } else if let Some(f) = &args.file {
        if args.text.is_some() {
            return Err(CliFail::usage("--file 不能与位置参数同时使用"));
        }
        Some(
            std::fs::read_to_string(f)
                .map_err(|e| CliFail::usage(format!("读取文件失败 {}: {e}", f.display())))?,
        )
    } else {
        args.text.clone()
    };

    // 图片：--image 上传 + --image-key 直用
    let mut img_keys = args.image_key.clone();
    for p in &args.image {
        img_keys.push(upload_local_image(&cfg, p)?);
    }

    let text = text.map(|t| t.trim_end_matches('\n').to_string());
    if text.as_deref().map(str::is_empty).unwrap_or(true) && img_keys.is_empty() {
        return Err(CliFail::usage("内容为空：提供文本、--stdin/--file 或 --image/--image-key"));
    }

    // 类型：强制 > 自动识别（与 APP detect 纯逻辑一致）
    let msg_type = match args.r#type.and_then(|t| t.wire()) {
        Some(t) => t,
        None => detect_type(text.as_deref().unwrap_or(""), img_keys.len()).t,
    };

    let key_refs: Vec<&str> = img_keys.iter().map(String::as_str).collect();
    let payload = build_payload(text.as_deref().unwrap_or(""), msg_type, &key_refs, args.title.as_deref())
        .map_err(CliFail::usage)?;

    // dry-run：只组装预览，不发送不写历史（D9：默认不出真实签名）
    if args.dry_run {
        let signed = qingniao_core::send::build_signed_payload(
            &default_secret_for_dry_run(&cfg, args.bot.as_deref()),
            &payload,
            now_secs(),
        );
        let sign_field = if args.show_sign {
            signed.get("sign").cloned().unwrap_or(Value::Null)
        } else {
            json!("omitted")
        };
        return Ok(if args.json {
            Outcome::ok_json(json!({
                "ok": true, "kind": "dry_run",
                "result": null, "error": null,
                "payload": payload, "sign": sign_field,
            }))
        } else {
            Outcome::ok_text(serde_json::to_string_pretty(&payload).unwrap())
        });
    }

    let recorded: RecordedSend =
        dispatch_send(&cfg, args.bot.as_deref(), &payload, args.allow_insecure_url, now_secs())
            .map_err(CliFail::from_send)?;

    // 历史条目补齐 APP 兼容字段（image 摘要 = 文件名列表；text/post 带 text 原文）
    let mut rec = recorded.record.clone();
    if msg_type == MsgType::Image && !args.image.is_empty() {
        let names: Vec<String> = args
            .image
            .iter()
            .map(|p| p.file_name().and_then(|n| n.to_str()).unwrap_or("image").to_string())
            .collect();
        rec["summary"] = json!(format!("{} ({} 张图)", names.join(", "), names.len()));
    }
    if matches!(msg_type, MsgType::Text | MsgType::Post) {
        if let Some(t) = &text {
            rec["text"] = json!(t);
        }
    }
    if let Err(e) = record_send(config_path, rec) {
        // 发送已完成，历史写失败只告警，不影响退出码
        warn_stderr(&format!("发送历史写入失败: {e}"));
    }

    match recorded.result {
        Ok(r) => {
            let code = if r.ok { 0 } else { 2 };
            if args.json {
                Ok(Outcome {
                    out: Out::Json(json!({
                        "ok": r.ok, "kind": "sent",
                        "result": {
                            "http_status": r.http_status,
                            "feishu_code": r.feishu_code,
                            "feishu_msg": r.feishu_msg,
                            "body_summary": r.body_summary,
                        },
                        "error": if r.ok { Value::Null } else { json!({
                            "kind": "feishu",
                            "message": recorded.status_line,
                        }) },
                        "payload": null, "sign": null,
                    })),
                    code,
                })
            } else if r.ok {
                Ok(Outcome::ok_text(format!("已发送: {}", recorded.status_line)))
            } else {
                // HTTP 层完成但业务失败（如飞书 code != 0）→ 退出码 2
                eprintln!("[青鸟] 错误(feishu): {}", recorded.status_line);
                if let Some(s) = &r.body_summary {
                    eprintln!("[青鸟] 响应: {s}");
                }
                Ok(Outcome { out: Out::Text(String::new()), code: 2 })
            }
        }
        Err(f) => Err(CliFail::from_send(f)),
    }
}

fn default_secret_for_dry_run(cfg: &Config, bot_key: Option<&str>) -> String {
    let bot = match bot_key {
        Some(k) => cfg.find_bot(k),
        None => cfg.default_bot(),
    };
    bot.map(|b| b.secret).unwrap_or_default()
}

// ===== bot =====

pub fn cmd_bot_ls(config_path: &Path, as_json: bool) -> Result<Outcome, CliFail> {
    let loaded = load_or_fail(config_path)?;
    print_warnings(&loaded.warnings);
    let bots = loaded.config.webhooks();
    let default_id = loaded.config.raw.get("last_bot_id").and_then(|v| v.as_str()).map(String::from);
    if as_json {
        Ok(Outcome::ok_json(json!(bots.iter().map(|b| json!({
            "id": b.id, "name": b.name, "url": b.url_masked(),
            "has_secret": b.has_secret(),
            "default": default_id.as_deref() == b.id.as_deref(),
        })).collect::<Vec<_>>())))
    } else {
        let mut lines = Vec::new();
        for b in &bots {
            let mark = if default_id.as_deref() == b.id.as_deref() { "*" } else { " " };
            lines.push(format!(
                "{mark} {:<2} {:<12} {}{}",
                (b.index + 1).to_string(),
                b.name,
                b.url_masked(),
                if b.has_secret() { "  [已配置签名密钥]" } else { "" }
            ));
        }
        Ok(Outcome::ok_text(if lines.is_empty() {
            "（无机器人，先用 `qingniao bot add` 或在 APP 中添加）".to_string()
        } else {
            lines.join("\n")
        }))
    }
}

pub fn cmd_bot_add(
    config_path: &Path,
    name: &str,
    url: &str,
    secret_stdin: bool,
    allow_insecure_url: bool,
    as_json: bool,
) -> Result<Outcome, CliFail> {
    check_url_policy(url, allow_insecure_url).map_err(CliFail::usage)?;
    let secret = if secret_stdin {
        read_secret_from_stdin()?
    } else {
        std::env::var("QINGNIAO_BOT_SECRET").unwrap_or_default()
    };
    let id = modify(config_path, |cfg| cfg.add_bot(name, url, &secret))
        .map_err(|e| CliFail::config(format!("添加机器人失败: {e}")))?;
    Ok(if as_json {
        Outcome::ok_json(json!({ "ok": true, "id": id, "name": name }))
    } else {
        Outcome::ok_text(format!("已添加: {name}（id={id}）"))
    })
}

pub fn cmd_bot_rm(config_path: &Path, key: &str, as_json: bool) -> Result<Outcome, CliFail> {
    let removed = modify(config_path, |cfg| cfg.remove_bot(key))
        .map_err(|e| CliFail::config(format!("删除机器人失败: {e}")))?;
    Ok(if as_json {
        Outcome::ok_json(json!({ "ok": true, "removed": { "id": removed.id, "name": removed.name } }))
    } else {
        Outcome::ok_text(format!("已删除: {}", removed.name))
    })
}

pub fn cmd_bot_use(config_path: &Path, key: &str, as_json: bool) -> Result<Outcome, CliFail> {
    let used = modify(config_path, |cfg| {
        let bot = cfg.find_bot(key).ok_or_else(|| format!("未找到机器人: {key}"))?;
        let id = bot
            .id
            .clone()
            .unwrap_or_else(|| format!("bot{}", bot.index + 1));
        cfg.set_last_bot(&id, bot.index);
        Ok(bot)
    })
    .map_err(|e| CliFail::config(format!("设置默认机器人失败: {e}")))?;
    Ok(if as_json {
        Outcome::ok_json(json!({ "ok": true, "default": { "id": used.id, "name": used.name } }))
    } else {
        Outcome::ok_text(format!("默认机器人已设为: {}", used.name))
    })
}

// ===== upload-image / config / history / doctor =====

pub fn cmd_upload_image(config_path: &Path, file: &Path, as_json: bool) -> Result<Outcome, CliFail> {
    let loaded = load_or_fail(config_path)?;
    print_warnings(&loaded.warnings);
    let key = upload_local_image(&loaded.config, file)?;
    Ok(if as_json {
        Outcome::ok_json(json!({ "ok": true, "image_key": key }))
    } else {
        Outcome::ok_text(key)
    })
}

pub fn cmd_config_show(config_path: &Path, as_json: bool) -> Result<Outcome, CliFail> {
    let loaded = load_or_fail(config_path)?;
    print_warnings(&loaded.warnings);
    let cfg = loaded.config;
    let default = cfg.default_bot();
    let doc = json!({
        "config_path": config_path.display().to_string(),
        "schema_version": cfg.schema_version(),
        "app_id": mask_app_id(&app_id_of(&cfg)),
        "has_app_secret": !app_secret_of(&cfg).is_empty(),
        "bots": cfg.webhooks().iter().map(|b| json!({
            "id": b.id, "name": b.name, "url": b.url_masked(), "has_secret": b.has_secret(),
        })).collect::<Vec<_>>(),
        "default_bot": default.as_ref().map(|b| json!({ "id": b.id, "name": b.name })),
        "history_count": cfg.history().len(),
    });
    Ok(if as_json {
        Outcome::ok_json(doc)
    } else {
        let mut lines = vec![
            format!("配置文件: {}", config_path.display()),
            format!("schema: v{}", cfg.schema_version()),
            format!("飞书凭证: app_id={} app_secret={}", mask_app_id(&app_id_of(&cfg)),
                if app_secret_of(&cfg).is_empty() { "未配置" } else { "已配置" }),
            format!("机器人: {} 个，默认: {}", cfg.webhooks().len(),
                default.map(|b| b.name).unwrap_or_else(|| "（未设置）".into())),
            format!("历史记录: {} 条", cfg.history().len()),
        ];
        for b in cfg.webhooks() {
            lines.push(format!(
                "  - {} {} {}{}",
                b.id.as_deref().unwrap_or("-"),
                b.name,
                b.url_masked(),
                if b.has_secret() { " [签名]" } else { "" }
            ));
        }
        Outcome::ok_text(lines.join("\n"))
    })
}

fn mask_app_id(app_id: &str) -> String {
    if app_id.is_empty() {
        return "（未配置）".into();
    }
    let head: String = app_id.chars().take(4).collect();
    format!("{head}****")
}

pub fn cmd_history(config_path: &Path, limit: usize, as_json: bool) -> Result<Outcome, CliFail> {
    let loaded = load_or_fail(config_path)?;
    print_warnings(&loaded.warnings);
    let records: Vec<Value> = loaded
        .config
        .history()
        .iter()
        .rev()
        .take(limit)
        .rev()
        .cloned()
        .collect();
    if as_json {
        Ok(Outcome::ok_json(json!(records.iter().map(|r| json!({
            "time": r.get("time"), "kind": r.get("kind"), "state": r.get("state"),
            "status": r.get("status"), "summary": r.get("summary"), "bot": r.get("bot"),
        })).collect::<Vec<_>>())))
    } else if records.is_empty() {
        Ok(Outcome::ok_text("（暂无发送记录）".to_string()))
    } else {
        let lines = records
            .iter()
            .map(|r| {
                format!(
                    "[{}] [{}] {} — {}",
                    r.get("time").and_then(|v| v.as_str()).unwrap_or("?"),
                    r.get("state").or_else(|| r.get("status")).and_then(|v| v.as_str()).unwrap_or("?"),
                    r.get("summary").and_then(|v| v.as_str()).unwrap_or(""),
                    r.get("bot").and_then(|v| v.as_str()).unwrap_or(""),
                )
            })
            .collect::<Vec<_>>();
        Ok(Outcome::ok_text(lines.join("\n")))
    }
}

pub fn cmd_doctor(config_path: &Path, as_json: bool) -> Result<Outcome, CliFail> {
    let mut checks: Vec<Value> = Vec::new();
    let mut failed = false;

    // 1. 配置文件
    let loaded = load(config_path);
    match &loaded {
        Ok(l) => {
            checks.push(check("config", true, format!("可读（{} 条历史，{} 个机器人）",
                l.config.history().len(), l.config.webhooks().len())));
            let sv = l.config.schema_version();
            checks.push(check("schema_version", sv <= 2, format!("v{sv}")));
            // 2. 机器人 URL 策略
            for b in l.config.webhooks() {
                let ok = check_url_policy(&b.url, false).is_ok();
                checks.push(check("bot-url", ok, format!("{} → {}", b.name, b.url_masked())));
                if !ok {
                    // 白名单外只是告警级（可用 --allow-insecure-url 放行），不算失败
                    let last = checks.last_mut().unwrap();
                    last["required"] = json!(false);
                }
            }
            if l.config.webhooks().is_empty() {
                checks.push(check("bots", false, "未配置机器人（qingniao bot add）"));
            }
            if !app_id_of(&l.config).is_empty() {
                checks.push(check("feishu-credentials", true, "飞书应用凭证已配置"));
            } else {
                checks.push(check("feishu-credentials", true, "未配置（仅影响图片上传）"));
                checks.last_mut().unwrap()["required"] = json!(false);
            }
        }
        Err(e) => {
            checks.push(check("config", false, e));
            failed = true;
        }
    }

    // 3. TCP 连通（无副作用：仅连接，不发消息）
    match tcp_probe("open.feishu.cn", 443) {
        Ok(()) => checks.push(check("network", true, "open.feishu.cn:443 可达")),
        Err(e) => {
            checks.push(check("network", false, format!("open.feishu.cn:443 不可达: {e}")));
        }
    }

    // 4. 安装路径（信息级）
    let install_path = expected_install_path();
    let installed = install_path.map(|p| p.exists()).unwrap_or(false);
    checks.push(check("cli-install", true, if installed {
        "已安装到本地 bin".to_string()
    } else {
        "未安装（APP「Agent」页一键安装，或手动复制二进制；不影响本命令运行）".to_string()
    }));
    checks.last_mut().unwrap()["required"] = json!(false);

    // 5. APP 并发提示（信息级，D3 已知限制）
    if app_running() {
        checks.push(check("app-running", true, "检测到 APP 正在运行：CLI 对配置的修改可能被 APP 保存覆盖，建议优先在 APP 内操作"));
        checks.last_mut().unwrap()["required"] = json!(false);
    }

    failed = failed || checks.iter().any(|c| {
        c["ok"].as_bool().unwrap_or(false) == false && c["required"].as_bool().unwrap_or(true)
    });

    let doc = json!({ "ok": !failed, "checks": checks });
    Ok(if as_json {
        Outcome { out: Out::Json(doc), code: if failed { 1 } else { 0 } }
    } else {
        let mut code = 0;
        let lines = checks.iter().map(|c| {
            let ok = c["ok"].as_bool().unwrap_or(false);
            let required = c["required"].as_bool().unwrap_or(true);
            let mark = if ok { "✓" } else if required { "✗" } else { "!" };
            if !ok && required {
                code = 1;
            }
            format!("{mark} {:<18} {}", c["check"].as_str().unwrap_or(""), c["detail"].as_str().unwrap_or(""))
        }).collect::<Vec<_>>();
        let _ = &doc;
        Outcome { out: Out::Text(lines.join("\n")), code }
    })
}

fn check(name: &str, ok: bool, detail: impl Into<String>) -> Value {
    json!({ "check": name, "ok": ok, "required": true, "detail": detail.into() })
}

fn tcp_probe(host: &str, port: u16) -> Result<(), String> {
    use std::net::{TcpStream, ToSocketAddrs};
    let addrs: Vec<_> = (host, port)
        .to_socket_addrs()
        .map_err(|e| format!("DNS 解析失败: {e}"))?
        .collect();
    for addr in addrs {
        if TcpStream::connect_timeout(&addr, std::time::Duration::from_secs(3)).is_ok() {
            return Ok(());
        }
    }
    Err("所有地址均连接失败".into())
}

fn expected_install_path() -> Option<PathBuf> {
    if cfg!(target_os = "windows") {
        std::env::var_os("LOCALAPPDATA")
            .map(|d| PathBuf::from(d).join("qingniao").join("bin").join("qingniao.exe"))
    } else {
        std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".local").join("bin").join("qingniao"))
    }
}

fn app_running() -> bool {
    #[cfg(target_os = "macos")]
    {
        std::process::Command::new("pgrep")
            .arg("-x")
            .arg("qingniao")
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false)
    }
    #[cfg(not(target_os = "macos"))]
    {
        false
    }
}
