//! M1 golden 测试：qingniao-core 对 M0 捕获的 JS 行为基线（tests/golden/golden.json）
//! 逐用例断言相等。基线由 `node scripts/capture-golden.mjs` 生成/校验（方案 v3 §四 D12）。

use qingniao_core::message::{
    build_payload, detect_type, extract_card_json, hmac_sign, md_to_post, parse_inline, MsgType,
};
use serde_json::{json, Value};
use std::path::PathBuf;

fn golden() -> Value {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../tests/golden/golden.json");
    let raw = std::fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("读取 golden 基线失败（{}）: {e}", path.display()));
    serde_json::from_str(&raw).expect("golden 基线必须是合法 JSON")
}

fn cases<'a>(g: &'a Value, group: &str) -> &'a Vec<Value> {
    g["cases"][group]
        .as_array()
        .unwrap_or_else(|| panic!("golden 缺少用例组 {group}"))
}

fn assert_eq_value(actual: Value, expected: &Value, group: &str, name: &str) {
    assert_eq!(
        &actual, expected,
        "{group}/{name} 与 JS 基线不一致\n actual: {actual}\n expected: {expected}"
    );
}

#[test]
fn golden_hmac_sign() {
    let g = golden();
    for c in cases(&g, "hmacSign") {
        let out = hmac_sign(
            c["input"]["secret"].as_str().unwrap(),
            c["input"]["timestamp"].as_str().unwrap(),
        );
        assert_eq_value(Value::String(out), &c["output"], "hmacSign", c["name"].as_str().unwrap());
    }
}

#[test]
fn golden_detect_type() {
    let g = golden();
    for c in cases(&g, "detectType") {
        let r = detect_type(
            c["input"]["text"].as_str().unwrap(),
            c["input"]["chips"].as_u64().unwrap() as usize,
        );
        assert_eq_value(
            json!({ "t": r.t.as_str(), "why": r.why }),
            &c["output"],
            "detectType",
            c["name"].as_str().unwrap(),
        );
    }
}

#[test]
fn golden_parse_inline() {
    let g = golden();
    for c in cases(&g, "parseInline") {
        let out = serde_json::to_value(parse_inline(c["input"]["text"].as_str().unwrap())).unwrap();
        assert_eq_value(out, &c["output"], "parseInline", c["name"].as_str().unwrap());
    }
}

#[test]
fn golden_md_to_post() {
    let g = golden();
    for c in cases(&g, "mdToPost") {
        let keys: Vec<String> = c["input"]["attImageKeys"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_str().unwrap().to_string())
            .collect();
        let keys: Vec<&str> = keys.iter().map(String::as_str).collect();
        let title = c["input"].get("titleOverride").and_then(|v| v.as_str());
        let out = md_to_post(c["input"]["md"].as_str().unwrap(), &keys, title);
        assert_eq_value(out, &c["output"], "mdToPost", c["name"].as_str().unwrap());
    }
}

#[test]
fn golden_extract_card_json() {
    let g = golden();
    for c in cases(&g, "extractCardJson") {
        let out = extract_card_json(c["input"]["text"].as_str().unwrap());
        assert_eq_value(out, &c["output"], "extractCardJson", c["name"].as_str().unwrap());
    }
}

#[test]
fn golden_build_payload() {
    let g = golden();
    for c in cases(&g, "buildPayload") {
        let name = c["name"].as_str().unwrap();
        let msg_type = MsgType::from_wire(c["input"]["type"].as_str().unwrap())
            .unwrap_or_else(|| panic!("buildPayload/{name}: 未知 wire type"));
        let keys: Vec<String> = c["input"]["imgKeys"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_str().unwrap().to_string())
            .collect();
        let keys: Vec<&str> = keys.iter().map(String::as_str).collect();
        let title = c["input"].get("title").and_then(|v| v.as_str());
        let result = build_payload(c["input"]["text"].as_str().unwrap(), msg_type, &keys, title);
        match (result, c.get("error")) {
            (Ok(v), None) => assert_eq_value(v, &c["output"], "buildPayload", name),
            (Err(e), Some(expected_err)) => {
                assert_eq!(&e, expected_err.as_str().unwrap(), "buildPayload/{name} 错误文案不一致");
            }
            (Ok(v), Some(_)) => panic!("buildPayload/{name}: 基线预期报错，实际成功: {v}"),
            (Err(e), None) => panic!("buildPayload/{name}: 基线预期成功，实际报错: {e}"),
        }
    }
}
