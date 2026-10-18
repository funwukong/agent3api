#!/usr/bin/env bash
# 发版收尾脚本：v* tag 的 build 工作流跑完后，一条命令挂上 GitHub Release。
#
#   bash scripts/release.sh vX.Y.Z            # 自动找该 tag 的成功 build run
#   bash scripts/release.sh vX.Y.Z <run-id>   # 或显式指定 run
#
# 做三件事：
#   1. 下载 build 工作流的 Windows 安装包 artifact（windows-nsis）到 dist/；
#   2. 用 tag 所指提交的提交信息（= 更新日志，见 agent.md 第 5 节）创建 / 更新
#      GitHub Release 并挂全部附件；
#   3. 打印验收提示。
#
# linux-server-* 两个 artifact 是后加的（容器内一键更新用）：老 tag 的 run 里
# 没有它们，脚本只提醒、不中断（历史 tag 仍可重跑收尾）。
#
# 幂等可重跑：附件 `--clobber` 覆盖上传。

set -euo pipefail

TAG="${1:?用法: scripts/release.sh vX.Y.Z [build-run-id]}"
RUN_ID="${2:-}"

case "$TAG" in
  v*) ;;
  *) echo "错误: '$TAG' 不是 v* 形式的 tag" >&2; exit 1 ;;
esac

# ── beta 版：tag 带预发布后缀（v3.1.0-beta.1）时，Release 必须标 pre-release ──
# 这个标记是应用里「加入 beta 版」开关的分界线：正式通道走 GitHub 的
# /releases/latest，它**排除**被标记的预发布；只有打开开关的人才从 /releases
# 列表里把 beta 捞出来。漏标 = beta 会被所有正式版用户当成正式更新收到；
# 给正式版误标则谁都不更新。所以这里按 tag 自动判、并顺手修正已存在的 Release。
case "${TAG#v}" in
  *-*) PRERELEASE=true ;;
  *)   PRERELEASE=false ;;
esac
if [ "$PRERELEASE" = true ]; then
  echo "→ 检测到预发布后缀（${TAG#v}），Release 标记为 pre-release（只发给「加入 beta 版」的用户）"
fi

command -v gh   >/dev/null || { echo "错误: 未安装 gh CLI" >&2; exit 1; }
# gh 的 JSON 输出用 node 解析（不引入 jq 依赖，两端都有 node）
command -v node >/dev/null || { echo "错误: 未安装 node" >&2; exit 1; }

# fork 仓库里 gh 默认可能识别 upstream；发版和上传 artifact 必须固定走 origin。
ORIGIN_URL=$(git remote get-url origin)
RELEASE_REPO=$(printf '%s\n' "$ORIGIN_URL" | sed -E 's#.*github\.com[:/]([0-9]+/)?##; s#\.git$##')
case "$RELEASE_REPO" in
  */*) ;;
  *) echo "错误: 无法从 origin URL 解析 GitHub 仓库: $ORIGIN_URL" >&2; exit 1 ;;
esac

# gh 是 Go 程序：既不读 git 的 http.proxy，也不读 Windows 系统代理，只认代理
# 环境变量。本机 git 配了代理而 gh 没配，下面的下载安装包与上传附件都是大文件
# 传输，直连不稳时会中途失败 —— 故把 git 的代理透传给 gh。
# 只设 HTTPS_PROXY：gh 的请求（含附件上传下载）全是 HTTPS，实测不读 HTTP_PROXY。
# 从 git 读取而非写死端口，换代理只改 git 一处；git 未配代理就不设，退回直连
# （外部已显式设过时不覆盖）。
if [ -z "${HTTPS_PROXY:-}" ]; then
  # 按 git/curl 查找代理的顺序依次尝试，兼容这几种配置写法：
  #   http.<url>.proxy（按仓库配） / http.proxy / https.proxy
  # 只取 --get-urlmatch 会漏掉 https.proxy，只取 https.proxy 会漏掉前两种。
  GH_PROXY=$(git config --get-urlmatch http.proxy https://github.com || true)
  [ -n "$GH_PROXY" ] || GH_PROXY=$(git config --get https.proxy || true)
  if [ -n "$GH_PROXY" ]; then
    export HTTPS_PROXY="$GH_PROXY"
    echo "→ 已启用代理: $GH_PROXY"
  fi
fi

# ── 1. 定位 build run 并下载安装包 ────────────────────────────
if [ -z "$RUN_ID" ]; then
  echo "→ 查找 $TAG 触发的 build 工作流…"
  # 不按 run 整体 conclusion 过滤：只验「两个安装包 artifact 是否齐全」，
  # 构建成功但别处失败的 run 里安装包照旧可用
  for id in $(gh run list --repo "$RELEASE_REPO" --workflow=build --limit 30 --json databaseId,headBranch \
      | node -e 'let s="";process.stdin.on("data",d=>s+=d);process.stdin.on("end",()=>{JSON.parse(s).filter(r=>r.headBranch===process.argv[1]).forEach(r=>process.stdout.write(r.databaseId+"\n"))})' "$TAG"); do
    if gh api "repos/$RELEASE_REPO/actions/runs/$id/artifacts" \
         --jq 'any(.artifacts[].name; . == "windows-nsis")' >/dev/null 2>&1; then
      RUN_ID=$id
      break
    fi
  done
  [ -n "$RUN_ID" ] || {
  echo "错误: 没找到 $TAG 的可用 build run（需 windows-nsis artifact）—— 先确认 CI 跑完（gh run list），或手动传 run-id" >&2
    exit 1
  }
fi
echo "→ build run: $RUN_ID"

rm -rf dist
gh run download "$RUN_ID" --repo "$RELEASE_REPO" -D dist
EXE=$(ls dist/windows-nsis/*.exe)
ASSETS=("$EXE")
echo "→ 已下载 $(basename "$EXE")"

# ── 2. 更新日志 = tag 所指提交的提交信息 ──────────────────────
NOTES_FILE=$(mktemp)
trap 'rm -f "$NOTES_FILE"' EXIT
git log -1 --format=%B "$TAG" > "$NOTES_FILE"

echo "→ GitHub Release"
if gh release view "$TAG" --repo "$RELEASE_REPO" >/dev/null 2>&1; then
  gh release upload "$TAG" --repo "$RELEASE_REPO" --clobber "${ASSETS[@]}"
  # 已存在时也把预发布标记纠正一遍（重跑收尾 / 当初漏标的情形都靠这一行自愈）
  gh release edit "$TAG" --repo "$RELEASE_REPO" --prerelease="$PRERELEASE"
  echo "  ↳ 已存在，附件覆盖上传（pre-release=$PRERELEASE）"
else
  gh release create "$TAG" --repo "$RELEASE_REPO" --prerelease="$PRERELEASE" --title "$TAG" \
    --notes-file "$NOTES_FILE" "${ASSETS[@]}"
  echo "  ↳ 已创建（pre-release=$PRERELEASE）"
fi

# ── 3. 验收提示 ───────────────────────────────────────────────
echo
echo "发版收尾完成。核对："
echo "  [1] GitHub Release:  gh release view $TAG --repo $RELEASE_REPO"
if [ "$PRERELEASE" = true ]; then
  echo "      ↑ beta 版：Release 应显示 Prerelease —— 正式版用户收不到它，"
  echo "        只有开了「加入 beta 版」的用户收得到（见 agent.md 第 4 节）"
else
  echo "  [2] Windows 安装包:  $EXE"
fi
echo "  [3] 安装包「关于」页版本号与 $TAG 一致"
