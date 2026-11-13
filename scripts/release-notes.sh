#!/usr/bin/env bash
# 生成 GitHub Release 正文：CHANGELOG 的手写段落 + 本版功能与变更提交标题。
# 多个平台作业都跑它，输出只依赖同一份 git 历史，后写正文不会覆盖出不同内容。
set -euo pipefail

ref="${1:-}"
if [ -z "$ref" ]; then
    echo "用法: $0 <tag 名或版本号>" >&2
    exit 2
fi
ver="${ref#v}"
case "$ref" in
    v*) tag_name="$ref" ;;
    *) tag_name="v$ref" ;;
esac

root=$(git rev-parse --show-toplevel 2>/dev/null) || {
    echo "不在 git 仓库内" >&2
    exit 1
}
cd "$root"

section=$(bash scripts/changelog-section.sh "$ver" 2>/dev/null || true)
if [ -n "$section" ]; then
    printf '%s\n' "$section"
fi

printf '\n<!-- commits:start -->\n## 本版提交 / Commits\n\n'

# 目标 tag 存在时以它为区间终点，本地预览历史版本也能得到正确范围。
if git rev-parse -q --verify "refs/tags/$tag_name" >/dev/null; then
    range_end="$tag_name"
else
    range_end=HEAD
fi

# 按提交历史距离取上一个 tag；区间终点上已有 tag 时退到父提交，避免范围归零。
head_tags=$(git tag --points-at "$range_end" 2>/dev/null || true)
prev_tag=$(git describe --tags --abbrev=0 "$range_end" 2>/dev/null || true)
if [ -n "$prev_tag" ] && [ -n "$head_tags" ] \
    && printf '%s\n' "$head_tags" | grep -qxF "$prev_tag"; then
    prev_tag=$(git describe --tags --abbrev=0 "$range_end^" 2>/dev/null || true)
fi

if [ -z "$prev_tag" ] || ! git rev-parse -q --verify "refs/tags/$prev_tag" >/dev/null; then
    printf '（取不到上一版 tag，跳过提交清单。）\n'
    exit 0
fi

count=$(git rev-list --count "$prev_tag..$range_end" 2>/dev/null || echo 0)
if [ "$count" = "0" ]; then
    printf '（自 `%s` 以来没有新提交。）\n' "$prev_tag"
    exit 0
fi

printf '自 `%s` 以来的功能与变更提交：\n\n' "$prev_tag"
# 版本提交、merge、修复类标题不进附录；修复内容由 CHANGELOG 手写段落承担。
git log --no-merges --format='- `%h` %s' "$prev_tag..$range_end" \
    | awk '
        /chore\(release\)/ { next }
        /` merge:/ { next }
        /修复/ { next }
        /(^|[^[:alnum:]])[Ff]ix[:(]/ { next }
        { print }
      '
printf '<!-- commits:end -->\n'
