#!/usr/bin/env bash
# 从 CHANGELOG.md 里抽出某个版本的段落，给 GitHub Release 当说明用。
#
# 用法：scripts/changelog-section.sh 0.7.9-beta1 > notes.md
#
# 为什么要有它：Release 的说明以前是**空的**（工作流没设 body，GitHub 也没自动生成），
# 发布页于是光秃秃的；而 CHANGELOG.md 里那份是**中英对照手写**的，正是给人看的。
# 现在 release.yml 的每个平台作业在建/更新 Release 前都跑一次这个脚本 ——
# 同一段内容写多次是幂等的，谁先跑完谁建 Release，内容都一样。
#
# 取法与「自动更新」对话框一致（`self_updater::changelog_section`）：`## [<版本>]` 开头，
# 到下一个 `## ` 之前结束；首尾空行去掉（中间的空行保留，Markdown 靠它分段）。
#
# ⚠️ tag 里那份 CHANGELOG 是**冻结**的：发完版别再回头改那一段，否则这里抽出来的
#    和用户当时在弹窗里看到的就对不上了。
set -eu

ver="${1:-}"
if [ -z "$ver" ]; then
  echo "用法: $0 <版本，可带 v 前缀>" >&2
  exit 2
fi
ver="${ver#v}"

awk -v want="[$ver]" '
  /^## / {
    if (inside) exit
    head = substr($0, 4)
    sub(/^[ \t]+/, "", head)
    if (index(head, want) == 1) inside = 1
    next
  }
  inside { lines[++n] = $0 }
  END {
    while (n > 0 && lines[n] ~ /^[ \t]*$/) n--
    start = 1
    while (start <= n && lines[start] ~ /^[ \t]*$/) start++
    for (i = start; i <= n; i++) print lines[i]
  }
' CHANGELOG.md
