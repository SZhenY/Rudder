#!/usr/bin/env bash
#
# 把本次发布的产物体积写进 GitHub Release 正文（幂等，可重复执行）。
#
# 用法：
#   scripts/size-report.sh <tag>              写进 Release 正文，并把表格打到 stdout
#   scripts/size-report.sh <tag> --dry-run    只生成表格，不写 Release
#
# 数据来源：
#   * 可执行文件体积：各构建作业上传的 size-*.json（压缩包里看不出未压缩大小）
#   * 安装包体积：Release 资产的 size（服务端权威值，AppImage / MSI 一并覆盖）
#   * 变化量：与上一份 Release 的同名资产对比（按 windows/linux/macos 起的后缀对齐，
#     版本号不同也能配上）
#
# 依赖：gh（已登录或带 GH_TOKEN）、awk/sed。CI 里由 size-report 作业调用。

set -euo pipefail

cd "$(git rev-parse --show-toplevel)"

# gh 在本仓库默认指向上游 meatshell，必须显式给仓库；CI 里 GITHUB_REPOSITORY 已就绪。
REPO="${GITHUB_REPOSITORY:-SZhenY/Rudder}"

TAG="${1:-}"
DRY_RUN=0
if [ "${2:-}" = "--dry-run" ]; then DRY_RUN=1; fi
if [ -z "$TAG" ]; then
    echo "用法: scripts/size-report.sh <tag> [--dry-run]" >&2
    exit 2
fi

BEGIN_MARK='<!-- size-report:start -->'
END_MARK='<!-- size-report:end -->'

# 字节 → 人读：>=1 MB 用 MB（一位小数），否则 KB。
human() {
    awk -v b="${1:-0}" 'BEGIN {
        if (b >= 1048576) printf "%.1f MB", b / 1048576;
        else printf "%.0f KB", b / 1024;
    }'
}

# 带符号的差值；缺基线时输出 —。
delta() {
    local cur="$1" prev="$2"
    if [ -z "$prev" ] || [ "$prev" = "0" ]; then
        printf '—'
        return
    fi
    awk -v c="$cur" -v p="$prev" 'BEGIN {
        d = c - p;
        sign = (d > 0) ? "+" : (d < 0 ? "−" : "±");
        if (d < 0) d = -d;
        if (d >= 1048576) printf "%s%.1f MB", sign, d / 1048576;
        else printf "%s%.0f KB", sign, d / 1024;
    }'
}

# 资产名按平台归一：去掉 "rudder-<版本>-" 前缀，保留 windows/linux/macos 起到结尾，
# 这样 v0.8.0-beta2 与 v0.8.0-beta3 的同名资产能配上。
asset_key() {
    sed -E 's/^.*-(windows|linux|macos)-/\1-/'
}

# 工作目录放 target/ 下（gitignored）。中间文件**不清理**：Windows 上删目录会被安全
# 策略拦截并刷一屏诊断，而这些文件在构建目录里本来就该被忽略。
tmpdir="target/size-report.tmp"
mkdir -p "$tmpdir"

current_tsv="$tmpdir/current.tsv"
gh release view "$TAG" -R "$REPO" --json assets -q '.assets[] | "\(.name)\t\(.size)"' > "$current_tsv"

prev_tag=$(gh release list -R "$REPO" -L 50 --json tagName -q '.[].tagName' |
    awk -v cur="$TAG" 'found { print; exit } $0 == cur { found = 1 }')
prev_tsv="$tmpdir/prev.tsv"
: > "$prev_tsv"
if [ -n "$prev_tag" ]; then
    gh release view "$prev_tag" -R "$REPO" --json assets -q '.assets[] | "\(.name)\t\(.size)"' > "$prev_tsv"
fi

# 上一版资产按归一化 key 建表（与 asset_key 同一套规则，只作用于文件名那一列）。
sed -E 's/^.*-(windows|linux|macos)-/\1-/' "$prev_tsv" > "$tmpdir/prev.map"

section="$tmpdir/section.md"
rows="$tmpdir/rows.md"
: > "$rows"
seen_assets="$tmpdir/seen"
: > "$seen_assets"
for f in sizes/*.json; do
    [ -e "$f" ] || continue
    name=$(sed -n 's/.*"name":"\([^"]*\)".*/\1/p' "$f")
    asset=$(sed -n 's/.*"asset":"\([^"]*\)".*/\1/p' "$f")
    bin=$(sed -n 's/.*"bin_bytes":\([0-9]*\).*/\1/p' "$f")
    [ -n "$name" ] || continue

    asset_size=$(awk -F'\t' -v a="$asset" '$1 == a { print $2 }' "$current_tsv")
    key=$(printf '%s' "$asset" | asset_key)
    prev_size=$(awk -F'\t' -v k="$key" '$1 == k { print $2 }' "$tmpdir/prev.map")
    printf '%s\n' "$asset" >> "$seen_assets"

    printf '| %s | %s | %s | %s |\n' \
        "$name" "$(human "${bin:-0}")" "$(human "${asset_size:-0}")" \
        "$(delta "${asset_size:-0}" "${prev_size:-}")" >> "$rows"
done

total=$(awk -F'\t' '{ s += $2 } END { print s + 0 }' "$current_tsv")

# 正式版的 Windows MSI 是安装版用户的自更新目标（`self_updater` 的 install_kind
# 判定为 MSI 时会挑 .msi）。缺了要在这里点名并以非 0 退出，把作业标红 ——
# 否则要等到用户点“立即更新”才发现安装包不存在。预发布版按设计不产 msi，
# 不参与检查。
msi_missing=""
case "$TAG" in
    *-alpha* | *-beta* | *-rc*) ;;
    *)
        for want in windows-x86_64.msi windows-aarch64.msi; do
            cut -f1 "$current_tsv" | grep -qE -- "-${want}$" || msi_missing="$msi_missing $want"
        done
        ;;
esac

{
    printf '## 包体积\n\n'
    printf '%s\n' "$BEGIN_MARK"
    printf '| 平台 | 可执行文件 | 安装包 | 与上一版相比 |\n'
    printf '|---|---:|---:|---:|\n'
    sort "$rows"

    # 没进 size-*.json 的资产（AppImage / MSI）单列，不参与对比。
    extras="$tmpdir/extras.md"
    : > "$extras"
    while IFS=$'\t' read -r name size; do
        [ -n "$name" ] || continue
        grep -qxF "$name" "$seen_assets" && continue
        printf '%s\n' "- \`$name\`：$(human "$size")" >> "$extras"
    done < "$current_tsv"
    if [ -s "$extras" ]; then
        printf '\n其他资产：\n\n'
        sort "$extras"
    fi

    printf '\n全部资产合计：%s（单个平台通常下载其中之一）。\n' "$(human "$total")"
    if [ -n "$msi_missing" ]; then
        printf '\n> ⚠ **正式版缺少 Windows MSI：%s** —— 安装版用户将无法自更新。\n' "$msi_missing"
    fi
    printf '%s\n' "$END_MARK"
} > "$section"

if [ "$DRY_RUN" = 1 ]; then
    [ -d sizes ] || echo "（未找到 sizes/*.json：只列安装包，可执行文件一列留空）" >&2
    cat "$section"
    # 预览也照样以非 0 退出，方便在发版前用退出码判断。
    [ -z "$msi_missing" ] || exit 1
    exit 0
fi

# 正文里已有这一段就整段替换，重跑不会堆积。
body="$tmpdir/body.md"
gh release view "$TAG" -R "$REPO" --json body -q .body |
    awk -v b="$BEGIN_MARK" -v e="$END_MARK" '
        index($0, b) { skip = 1; next }
        index($0, e) { skip = 0; next }
        !skip { print }
    ' |
    awk 'NF { last = NR } { line[NR] = $0 } END { for (i = 1; i <= last; i++) print line[i] }' \
        > "$body"

{
    cat "$body"
    printf '\n\n'
    cat "$section"
} > "$tmpdir/new-body.md"

# 更新 Release 只能按 release id：`PATCH /releases/tags/<tag>` 不存在，会 404。
release_id=$(gh api "repos/$REPO/releases/tags/$TAG" -q .id)
gh api --method PATCH "repos/$REPO/releases/$release_id" \
    -F "body=@$tmpdir/new-body.md" > /dev/null
echo "已写入 $TAG 的发布说明（基线：${prev_tag:-无}）。" >&2
cat "$section"

if [ -n "$msi_missing" ]; then
    echo "" >&2
    echo "错误：正式版缺少 Windows MSI 资产：$msi_missing" >&2
    echo "      安装版用户的自更新依赖它，见 .github/workflows/release.yml 的 Build MSI installer。" >&2
    exit 1
fi
