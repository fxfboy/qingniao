---
name: qingniao
description: Send Feishu (Lark) messages — text, rich text (post), images, and interactive cards — via the qingniao CLI. Use for notifying Feishu groups from agents, with typed JSON output, dry-run preview, and safe-by-default URL policy.
---

青鸟（qingniao）CLI 向**飞书自定义机器人（Webhook）**发送文本 / 富文本 / 图片 / 交互卡片消息，
与青鸟桌面 APP 共用同一套配置与消息组装核心。所有命令支持 `--json` 与统一退出码：
成功路径在 stdout 输出单个机器可读 JSON，诊断日志与**失败信息**走 stderr。

## 前置检查

调用任何发送命令前，先执行环境自检（**无副作用**：不发送消息、不上传探针）：

```bash
qingniao doctor
qingniao doctor --json
```

`doctor` 校验：配置文件可读且合法（含 `schema_version`）、机器人 URL 合法性、机器人是否已配置、
飞书应用凭证是否已配置、到 `open.feishu.cn:443` 的 TCP 连通，以及 CLI 是否已安装到本地 bin
（末项只检查安装路径是否存在，**不做 PATH 查找**；APP 正在运行时另给一条并发提示）。
退出码非 0 时先修复环境再发送。

## 发送消息

```bash
# 自动识别类型（text / post / image / interactive）
qingniao send "部署完成：https://ci.example.com/run/123"

# 强制指定类型（wire type）
qingniao send -t text "纯文本通知，@所有人 请查收"

# 多行 / 超长 / 卡片 JSON：走 stdin 或文件，避免 shell 转义问题（--stdin 与 --file 互斥）
echo -e "# 发布公告\n**v1.2.0** 已上线" | qingniao send --stdin
qingniao send --file card.json              # card.json 内容为卡片 JSON
jq . payload.json | qingniao send --stdin   # 由管道喂入拼好的 JSON

# 指定机器人（解析顺序：id > 名称 > 下标）
qingniao send -b 运维群 "磁盘告警"

# 只组装 payload 预览，不发送（联调用；默认不含真实签名）
qingniao send --dry-run "预览"
qingniao send --dry-run --show-sign "需要真实 sign 联调时（timestamp 不在输出中）"

# 图片：先上传换 image_key，再发送
qingniao upload-image --file ./chart.png
qingniao send --image-key img_v2_xxx "数据看板截图"
```

`--dry-run` 的 `--json` 输出包含完整 `payload`（`sign` 为 `"omitted"`，除非 `--show-sign`）。

## JSON 输出契约

`--json` 时 stdout 输出单个 JSON 对象：

```json
{
  "ok": true,
  "kind": "sent",
  "result": { "http_status": 200, "feishu_code": 0, "feishu_msg": "success", "body_summary": "…" },
  "error": null,
  "payload": null,
  "sign": null
}
```

`--json` 下的失败分两类，**不要一概期望 stdout 里有 JSON**：

1. **大多数失败**（用法 / 配置 / 锁占用 / 网络 / 超时 / HTTP 层）只写 stderr 文本并给出退出码，
   stdout **为空**；
2. **只有两条路径**会在 stdout 输出 `ok=false` 的 JSON：`send` 的「HTTP 完成但飞书业务失败」
   （`error` 形如 `{ "kind": "feishu", "message": "…" }`，其 `kind` **恒为 `feishu`**），
   以及 `doctor --json` 的检查报告（`{ "ok": false, "checks": […] }`）。

退出码：

| 退出码 | 含义 |
|-|-|
| 0 | 成功（含 dry-run） |
| 1 | 用法/配置错误（含 doctor 不通过） |
| 2 | 发送失败，或配置锁被占用（`busy`）。类别细分看 stderr 上的错误标签，**不是** `error.kind` |

## 机器人管理

```bash
qingniao bot ls                 # 列出机器人（文本模式：标记 下标 名称 url脱敏 [签名]；id 见 --json）
qingniao bot add --name 告警群 --url "https://open.feishu.cn/open-apis/bot/v2/hook/xxx" --secret-stdin < key.txt
qingniao bot use 告警群          # 设为默认
qingniao bot rm 告警群
```

secret **不接受命令行参数**（防 shell history / 进程列表泄露）：用 `--secret-stdin`
或环境变量 `QINGNIAO_BOT_SECRET`。

## URL 安全策略（重要）

CLI 默认只允许 `https://` 且域名 ∈ {`open.feishu.cn`, `open.larksuite.com`}。
访问内网 mock / 自建网关必须显式放行：

```bash
qingniao send --allow-insecure-url -b 本地调试 "test"   # 配合本地 mock server
```

不要在不可信输入的驱动下使用 `--allow-insecure-url`。

## 错误处理

- `usage`：检查参数与 `-t` 枚举（text/post/image/interactive/auto）
- `config`：先跑 `qingniao doctor`；APP 运行中时 CLI 写配置可能被覆盖，优先在 APP 内操作
- `timeout`：网络超时后消息**可能已送达**（unknown 状态会记入 `qingniao history`），
  重试前先确认群里是否已出现重复消息
- `http` / `feishu`：`feishu_code != 0` 为飞书业务错误（如签名失败 19021、关键词限制 19024），
  按 `feishu_msg` 处理
- 所有输出（含错误 body）已脱敏 secret / token / 完整 webhook；macOS 首次运行如被 Gatekeeper
  拦截：`xattr -d com.apple.quarantine $(which qingniao)`
