#!/bin/bash
# =============================================================
# 青鸟 · 飞书文档 → 本地 docs/ 单向同步脚本
# 唯一真源：飞书知识库「青鸟项目」
# 本地 docs/ 仅作只读镜像（已被 .gitignore 忽略，不进 git）
#
# 用法：
#   ./scripts/sync-docs.sh            # 同步全部文档
#   ./scripts/sync-docs.sh --check    # 只检查 lark-cli 可用性
#
# 身份：默认 --as bot（机器人身份对这些文档有读权限，且不需要用户 token）；
# 若要以本人身份同步（例如机器人权限被收回），用 LARK_AS=user 运行，
# 并先 `lark-cli auth login --domain docs,drive,wiki`.
# =============================================================
set -euo pipefail

# 项目根目录（脚本所在目录的上一级）
ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
DOCS_DIR="${ROOT_DIR}/docs"
LARK_AS="${LARK_AS:-bot}"

# 知识库文档清单：本地文件名 | 飞书 docx URL
# 新增文档时在此追加一行即可
DOC_MAP=(
  "需求列表|https://fglabtop.feishu.cn/docx/Bz61dxk3SoxChtx1TTccUI1snUg"
  "Agent-CLI-方案|https://fglabtop.feishu.cn/docx/Z4MhdOvEpo8ppAxmQHpcKnVVnEg"
  "Agent-CLI-方案-review|https://fglabtop.feishu.cn/docx/JMNcd0JmKoz4rCxHpvqcWuapnkg"
  "文件传输功能设计文档|https://fglabtop.feishu.cn/docx/ACRedaZCio7uJAxfSLncSGG9nVc"
  "CLI 文件传输方案|https://fglabtop.feishu.cn/docx/KCz2d3wrVoiKKtxZDy3cP9RGncg"
  "CLI 文件传输方案-review|https://fglabtop.feishu.cn/docx/LFQzd3JBso3tXRxkNBvcJ1EhnGe"
  "dock-tray 常驻功能设计文档|https://fglabtop.feishu.cn/docx/L7skdmr8GoyL2Lx9Kt1c7Bk6n8b"
  "文件传输功能设计文档-review|https://fglabtop.feishu.cn/docx/WI3Pd1wJhocgM3xoyHRc3gVUndc"
  "测试用例清单|https://fglabtop.feishu.cn/docx/L4jFda9aho3Vm0xJKNWc67dhnMh"
  "测试报告|https://fglabtop.feishu.cn/docx/RseMdoKeaoBNUNxEVDKc2FXRnVc"
  "项目导航|https://fglabtop.feishu.cn/docx/XHmadlsv2oCzrfxfm6fckIKLnab"
  "架构图|https://fglabtop.feishu.cn/docx/P0kOdhpsloNsklxK9S9cVR0gnPe"
  "分片AAD协议偏差说明|https://fglabtop.feishu.cn/docx/CXrAdRVfJoMip5xtmhvcch3Rnaf"
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

# lark-cli 只接受「当前目录内的相对输出路径」，绝对路径会被判为 unsafe output path
cd "${ROOT_DIR}"

echo "==> 开始同步飞书文档到 ${DOCS_DIR}（身份：${LARK_AS}）"

# 先导出到临时文件再与本地比对：本地版与飞书不同就先备份，避免静默覆盖本地改动。
# 背景：2026-09-18 事故——「只改本地、待回贴飞书」的两次编辑
# （Agent-CLI-方案 v3、dock-tray 常驻功能设计文档 v0.6）被一次全量同步无提示覆盖，
# docs/ 又在 .gitignore 内，没有 git 历史可恢复，只能靠编辑器 file-history 抢救。
BACKUP_DIR="${DOCS_DIR}/.bak"
TMP_NAME="_qn-sync-tmp.md"

ok_count=0
backup_count=0
for entry in "${DOC_MAP[@]}"; do
  name="${entry%%|*}"
  url="${entry##*|}"
  # lark-cli ≥1.0.47 的 drive +export 只接受 --token/--doc-type（旧版 --url 已移除），
  # 这里从 URL 解析：https://<tenant>.feishu.cn/<doc-type>/<token>
  token="${url##*/}"
  case "${url}" in
    */docx/*)   doc_type="docx" ;;
    */doc/*)    doc_type="doc" ;;
    */sheets/*) doc_type="sheet" ;;
    */base/*)   doc_type="bitable" ;;
    */slides/*) doc_type="slides" ;;
    *)          doc_type="docx" ;;
  esac
  echo "  - 导出：${name}（${doc_type}）"
  if err=$(lark-cli drive +export \
      --token "${token}" \
      --doc-type "${doc_type}" \
      --file-extension markdown \
      --file-name "${TMP_NAME}" \
      --output-dir docs \
      --overwrite \
      --as "${LARK_AS}" 2>&1); then
    target="${DOCS_DIR}/${name}.md"
    if [[ -f "${target}" ]] && ! cmp -s "${DOCS_DIR}/${TMP_NAME}" "${target}"; then
      mkdir -p "${BACKUP_DIR}"
      cp -p "${target}" "${BACKUP_DIR}/${name}.$(date +%Y%m%d-%H%M%S).md"
      backup_count=$((backup_count + 1))
      echo "    [备份] 本地版与飞书不一致，旧版已存到 docs/.bak/"
    fi
    mv "${DOCS_DIR}/${TMP_NAME}" "${target}"
    ok_count=$((ok_count + 1))
  else
    # 打印真实报错，否则出问题只能看到「导出失败」
    echo "    [失败] ${name}：$(printf '%s' "${err}" | tr '\n' ' ' | cut -c1-300)" >&2
  fi
done
rm -f "${DOCS_DIR}/${TMP_NAME}"

echo "==> 同步完成：${ok_count}/${#DOC_MAP[@]} 份文档已更新"
if (( backup_count > 0 )); then
  echo "==> 有 ${backup_count} 份本地版与飞书不同，已备份到 ${BACKUP_DIR}（确认无用后可删）"
fi
