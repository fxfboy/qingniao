#!/bin/bash
# =============================================================
# 青鸟 · 飞书文档 → 本地 docs/ 单向同步脚本
# 唯一真源：飞书知识库「青鸟项目」
# 本地 docs/ 仅作只读镜像（已被 .gitignore 忽略，不进 git）
#
# 用法：
#   ./scripts/sync-docs.sh            # 同步全部文档
#   ./scripts/sync-docs.sh --check    # 只检查 lark-cli 可用性
# =============================================================
set -euo pipefail

# 项目根目录（脚本所在目录的上一级）
ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
DOCS_DIR="${ROOT_DIR}/docs"

# 知识库文档清单：本地文件名 | 飞书 docx URL
# 新增文档时在此追加一行即可
DOC_MAP=(
  "需求列表|https://fglabtop.feishu.cn/docx/Bz61dxk3SoxChtx1TTccUI1snUg"
  "Agent-CLI-方案|https://fglabtop.feishu.cn/docx/Z4MhdOvEpo8ppAxmQHpcKnVVnEg"
  "Agent-CLI-方案-review|https://fglabtop.feishu.cn/docx/JMNcd0JmKoz4rCxHpvqcWuapnkg"
  "文件传输功能设计文档|https://fglabtop.feishu.cn/docx/ACRedaZCio7uJAxfSLncSGG9nVc"
  "dock-tray 常驻功能设计文档|https://fglabtop.feishu.cn/docx/L7skdmr8GoyL2Lx9Kt1c7Bk6n8b"
  "文件传输功能设计文档-review|https://fglabtop.feishu.cn/docx/WI3Pd1wJhocgM3xoyHRc3gVUndc"
  "测试用例清单|https://fglabtop.feishu.cn/docx/L4jFda9aho3Vm0xJKNWc67dhnMh"
  "测试报告|https://fglabtop.feishu.cn/docx/RseMdoKeaoBNUNxEVDKc2FXRnVc"
  "项目导航|https://fglabtop.feishu.cn/docx/XHmadlsv2oCzrfxfm6fckIKLnab"
)

# 检查 lark-cli
if ! command -v lark-cli >/dev/null 2>&1; then
  echo "[错误] 未找到 lark-cli，请先配置飞书 CLI 环境" >&2
  exit 1
fi

if [[ "${1:-}" == "--check" ]]; then
  echo "[OK] lark-cli 可用，共 ${#DOC_MAP[@]} 份文档待同步"
  exit 0
fi

mkdir -p "${DOCS_DIR}"

echo "==> 开始同步飞书文档到 ${DOCS_DIR}"

ok_count=0
for entry in "${DOC_MAP[@]}"; do
  name="${entry%%|*}"
  url="${entry##*|}"
  echo "  - 导出：${name}"
  if lark-cli drive +export \
      --url "${url}" \
      --file-extension markdown \
      --file-name "${name}.md" \
      --output-dir "${DOCS_DIR}" \
      --overwrite \
      --as user >/dev/null 2>&1; then
    ok_count=$((ok_count + 1))
  else
    echo "    [失败] ${name} 导出失败，跳过" >&2
  fi
done

echo "==> 同步完成：${ok_count}/${#DOC_MAP[@]} 份文档已更新"
