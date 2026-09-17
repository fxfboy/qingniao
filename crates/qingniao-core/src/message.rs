//! 消息类型识别与组装：`src/message.js` 六个纯函数的 Rust 移植。
//!
//! 移植纪律（方案 v3 §四 D12）：以当前 JS 行为为兼容基线，已知缺陷
//! （代码块无样式、单星斜体缺失、卡片正则误判等）原样保留，登记为独立修复项。

use regex::Regex;
use serde::Serialize;
use serde_json::{json, Value};
use std::sync::OnceLock;

fn re(pattern: &'static str) -> Regex {
    Regex::new(pattern).expect("内置正则必须可编译")
}

// JS 源：src/message.js detect() 中的四个正则与 parseInline 的前缀正则。
// JS 的 ^ 在非 multiline 下为串首、search 为最左匹配，与 regex crate 默认语义一致。
fn md_detect_re() -> &'static Regex {
    static R: OnceLock<Regex> = OnceLock::new();
    R.get_or_init(|| {
        re(r"(?m)(\*\*[^*]+\*\*|`[^`]+`|\[[^\]]+\]\([^)]+\)|^#{1,3}\s|^[-*]\s|^>\s|^\d+\.\s)")
    })
}
fn at_detect_re() -> &'static Regex {
    static R: OnceLock<Regex> = OnceLock::new();
    R.get_or_init(|| re(r"@所有人|@[^\s@]{1,12}"))
}
fn card_detect_re() -> &'static Regex {
    static R: OnceLock<Regex> = OnceLock::new();
    R.get_or_init(|| re(r#"```card|\{\s*"config"|\{\s*"header""#))
}
fn at_tag_re() -> &'static Regex {
    static R: OnceLock<Regex> = OnceLock::new();
    R.get_or_init(|| re(r#"^<at\s+user_id="([^"]+)"[^>]*>([^<]*)</at>"#))
}
fn img_re() -> &'static Regex {
    static R: OnceLock<Regex> = OnceLock::new();
    R.get_or_init(|| re(r"^!\[([^\]]*)\]\(([^)]+)\)"))
}
fn link_re() -> &'static Regex {
    static R: OnceLock<Regex> = OnceLock::new();
    R.get_or_init(|| re(r"^\[([^\]]+)\]\(([^)]+)\)"))
}
fn bold_re() -> &'static Regex {
    static R: OnceLock<Regex> = OnceLock::new();
    R.get_or_init(|| re(r"^\*\*([^*]+)\*\*"))
}
fn code_re() -> &'static Regex {
    static R: OnceLock<Regex> = OnceLock::new();
    R.get_or_init(|| re(r"^`([^`]+)`"))
}
fn next_special_re() -> &'static Regex {
    static R: OnceLock<Regex> = OnceLock::new();
    R.get_or_init(|| re(r"(\*\*|\[|<at|@所有人|!\[|`)"))
}
fn card_fence_start_re() -> &'static Regex {
    static R: OnceLock<Regex> = OnceLock::new();
    R.get_or_init(|| re(r"(?i)^```card\s*"))
}
fn card_fence_end_re() -> &'static Regex {
    static R: OnceLock<Regex> = OnceLock::new();
    R.get_or_init(|| re(r"```\s*$"))
}

/// 消息 wire 类型（-t 枚举取 wire type，方案 v3 §六）
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum MsgType {
    Text,
    Post,
    Image,
    Interactive,
}

impl MsgType {
    pub fn as_str(&self) -> &'static str {
        match self {
            MsgType::Text => "text",
            MsgType::Post => "post",
            MsgType::Image => "image",
            MsgType::Interactive => "interactive",
        }
    }
    pub fn from_wire(s: &str) -> Option<MsgType> {
        match s {
            "text" => Some(MsgType::Text),
            "post" => Some(MsgType::Post),
            "image" => Some(MsgType::Image),
            "interactive" => Some(MsgType::Interactive),
            _ => None,
        }
    }
}

/// 类型识别结果；`why` 为兼容基线内的 HTML 片段（前端直接渲染，CLI 侧另行映射纯文本，
/// 方案 v3 §三 D15：Rust 对外契约将提供 reason_codes，此处先保真迁移现状）
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct DetectResult {
    pub t: MsgType,
    pub why: String,
}

/// JS 源：src/message.js detect() 纯逻辑部分
pub fn detect_type(text: &str, chips: usize) -> DetectResult {
    let has_md = md_detect_re().is_match(text);
    let has_at = at_detect_re().is_match(text);
    if card_detect_re().is_match(text) {
        return DetectResult {
            t: MsgType::Interactive,
            why: "检测到 <b>卡片 JSON</b>，将以交互卡片渲染".to_string(),
        };
    }
    if chips > 0 && text.trim().is_empty() {
        return DetectResult {
            t: MsgType::Image,
            why: format!("仅包含 <b>{chips} 张图片</b>，采用图片消息发送"),
        };
    }
    if chips > 0 || has_md || has_at {
        let mut parts: Vec<String> = Vec::new();
        if chips > 0 {
            parts.push(format!("{chips} 张图片"));
        }
        if has_md {
            parts.push("Markdown".to_string());
        }
        if has_at {
            parts.push("@ 提及".to_string());
        }
        return DetectResult {
            t: MsgType::Post,
            why: format!("检测到 <b>{}</b>，纯文本无法完整呈现", parts.join("、")),
        };
    }
    if text.trim().is_empty() {
        return DetectResult {
            t: MsgType::Text,
            why: "编辑器为空 · 输入内容后会自动判断消息类型".to_string(),
        };
    }
    DetectResult {
        t: MsgType::Text,
        why: "纯文字消息，直接以 text 类型发送".to_string(),
    }
}

/// post 行内元素（serde 内部 tag 对齐 JS 的 `{tag, ...}` 结构）
#[derive(Debug, Clone, Serialize, PartialEq)]
#[serde(tag = "tag")]
pub enum Inline {
    #[serde(rename = "text")]
    Text { text: String },
    #[serde(rename = "a")]
    A {
        text: String,
        href: String,
    },
    #[serde(rename = "img")]
    Img { image_key: String },
    #[serde(rename = "at")]
    At { user_id: String, user_name: String },
}

/// JS 源：src/message.js parseInline()
///
/// JS 按 UTF-16 码元推进（`text[i]` 取单个码元）；Rust 按字符推进。差异仅在
/// 「增补平面字符紧邻标记符」的切分场景可见（JS 会产出孤立代理对），属基线外的
/// 已知 JS 缺陷，Rust 侧取字符语义（更正确），见方案 v3 §四。
pub fn parse_inline(text: &str) -> Vec<Inline> {
    let mut out: Vec<Inline> = Vec::new();
    let mut i = 0usize;
    while i < text.len() {
        let rest = &text[i..];
        if let Some(c) = at_tag_re().captures(rest) {
            out.push(Inline::At {
                user_id: c[1].to_string(),
                user_name: c[2].to_string(),
            });
            i += c.get(0).unwrap().end();
            continue;
        }
        if rest.starts_with("@所有人") {
            out.push(Inline::At {
                user_id: "all".to_string(),
                user_name: "所有人".to_string(),
            });
            i += "@所有人".len();
            continue;
        }
        if let Some(c) = img_re().captures(rest) {
            out.push(Inline::Img {
                image_key: c[2].to_string(),
            });
            i += c.get(0).unwrap().end();
            continue;
        }
        if let Some(c) = link_re().captures(rest) {
            out.push(Inline::A {
                text: c[1].to_string(),
                href: c[2].to_string(),
            });
            i += c.get(0).unwrap().end();
            continue;
        }
        if let Some(c) = bold_re().captures(rest) {
            out.push(Inline::Text {
                text: c[1].to_string(),
            });
            i += c.get(0).unwrap().end();
            continue;
        }
        if let Some(c) = code_re().captures(rest) {
            out.push(Inline::Text {
                text: c[1].to_string(),
            });
            i += c.get(0).unwrap().end();
            continue;
        }
        match next_special_re().find(rest) {
            None => {
                out.push(Inline::Text {
                    text: rest.to_string(),
                });
                break;
            }
            Some(m) => {
                let next = m.start();
                if next > 0 {
                    out.push(Inline::Text {
                        text: rest[..next].to_string(),
                    });
                    i += next;
                } else {
                    let ch = rest.chars().next().unwrap();
                    out.push(Inline::Text {
                        text: ch.to_string(),
                    });
                    i += ch.len_utf8();
                }
            }
        }
    }
    out
}

fn flush_code_lines(content: &mut Vec<Value>, code_lines: &[&str]) {
    for cl in code_lines {
        content.push(json!([{ "tag": "text", "text": if cl.is_empty() { " " } else { cl } }]));
    }
}

/// JS 源：src/message.js mdToPost()（含基线缺陷：代码块剥 fence 按纯文本、引用剥 `>` 无样式）
pub fn md_to_post(md: &str, att_image_keys: &[&str], title_override: Option<&str>) -> Value {
    let mut title = title_override.unwrap_or("").to_string();
    let mut content: Vec<Value> = Vec::new();
    let mut in_code_block = false;
    let mut code_lines: Vec<&str> = Vec::new();
    for line in md.split('\n') {
        if line.trim().starts_with("```") {
            if in_code_block {
                flush_code_lines(&mut content, &code_lines);
                code_lines.clear();
                in_code_block = false;
            } else {
                in_code_block = true;
            }
            continue;
        }
        if in_code_block {
            code_lines.push(line);
            continue;
        }
        if title.is_empty() && line.starts_with("# ") {
            title = line[2..].trim().to_string();
            continue;
        }
        if line.trim().is_empty() {
            continue;
        }
        let mut processed = line;
        if let Some(stripped) = processed.strip_prefix("> ") {
            processed = stripped;
        } else if let Some(stripped) = processed.strip_prefix('>') {
            processed = stripped;
        }
        let inline = parse_inline(processed);
        if !inline.is_empty() {
            content.push(serde_json::to_value(&inline).expect("Inline 序列化不可失败"));
        }
    }
    if in_code_block && !code_lines.is_empty() {
        flush_code_lines(&mut content, &code_lines);
    }
    for key in att_image_keys {
        content.push(json!([{ "tag": "img", "image_key": key }]));
    }
    json!({
        "msg_type": "post",
        "content": { "post": { "zh_cn": { "title": title, "content": content } } }
    })
}

/// JS 源：src/message.js extractCardJson()（含基线缺陷：非 JSON 输入自动包装成默认卡片）
pub fn extract_card_json(text: &str) -> Value {
    let trimmed = text.trim();
    let t = card_fence_start_re().replace(trimmed, "");
    let t = card_fence_end_re().replace(&t, "");
    let t = t.trim();
    if let Ok(v) = serde_json::from_str::<Value>(t) {
        return v;
    }
    let lines: Vec<&str> = text
        .trim()
        .split('\n')
        .filter(|l| !l.trim().is_empty())
        .collect();
    let mut card = json!({ "config": { "wide_screen_mode": true }, "elements": [] });
    if !lines.is_empty() {
        let title = lines[0];
        let body = lines[1..].join("\n");
        card["header"] =
            json!({ "title": { "tag": "plain_text", "content": title }, "template": "blue" });
        if !body.is_empty() {
            card["elements"]
                .as_array_mut()
                .expect("上面 json! 保证 elements 是数组")
                .push(json!({ "tag": "div", "text": { "tag": "lark_md", "content": body } }));
        }
    }
    card
}

/// JS 源：src/message.js buildPayload()。
/// 未知类型在 JS 中返回 undefined（上游崩溃），Rust 侧用 MsgType 枚举杜绝该状态。
pub fn build_payload(
    text: &str,
    msg_type: MsgType,
    img_keys: &[&str],
    title: Option<&str>,
) -> Result<Value, String> {
    match msg_type {
        MsgType::Text => Ok(json!({ "msg_type": "text", "content": { "text": text } })),
        MsgType::Post => Ok(md_to_post(text, img_keys, title)),
        MsgType::Image => {
            if img_keys.is_empty() {
                return Err("没有可用的 image_key，请等待图片上传完成".to_string());
            }
            Ok(json!({ "msg_type": "image", "content": { "image_key": img_keys[0] } }))
        }
        MsgType::Interactive => {
            let mut card = extract_card_json(text);
            if !img_keys.is_empty()
                && card.get("elements").map(|e| e.is_array()).unwrap_or(false)
            {
                let arr = card["elements"].as_array_mut().unwrap();
                for key in img_keys {
                    arr.push(json!({
                        "tag": "img", "img_key": key,
                        "alt": { "tag": "plain_text", "content": "图片" }
                    }));
                }
            }
            Ok(json!({ "msg_type": "interactive", "card": card }))
        }
    }
}

/// JS 源：src/message.js hmacSign()。
/// 飞书自定义机器人签名：key = `timestamp + "\n" + secret`，对空 payload 计算 HMAC-SHA256，base64 输出。
pub fn hmac_sign(secret: &str, timestamp: &str) -> String {
    use base64::Engine;
    use hmac::Mac;
    let mut mac = hmac::Hmac::<sha2::Sha256>::new_from_slice(
        format!("{timestamp}\n{secret}").as_bytes(),
    )
    .expect("HMAC 接受任意长度 key");
    mac.update(b"");
    base64::engine::general_purpose::STANDARD.encode(mac.finalize().into_bytes())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sign_matches_feishu_doc_example() {
        // 飞书官方文档示例向量（verify-core.js 同源）
        assert_eq!(
            hmac_sign("test-secret", "1700000000"),
            "mbm4Y4oluIPQ00qlBIhX8vAZ0EKv3nw0LuTb91jPL84="
        );
    }

    #[test]
    fn parse_inline_basic() {
        assert_eq!(
            parse_inline("**加粗**普通"),
            vec![
                Inline::Text { text: "加粗".into() },
                Inline::Text { text: "普通".into() },
            ]
        );
    }
}
