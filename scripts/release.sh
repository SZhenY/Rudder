#!/usr/bin/env bash
#
# Rudder 发版脚本（macOS / Linux）。Windows 用 scripts/release.ps1，两者语义一致。
#
# 版本形态（见 docs/release-process.md）：
#   X.Y.Z          正式版（把 CHANGELOG 的 [Unreleased] 提升为这一版）
#   X.Y.Z-fixN     补丁版：正式版之后、下一个小版本之前的紧急修复，N 从 1 递增
#   X.Y.Z-alphaN / -betaN / -rcN
#                  预发布版：与 -fixN 同一种思路的不同成熟度（正式版之上继续做出来的
#                  构建），同样把 [Unreleased] 提升为这一版
#
# 用法：
#   scripts/release.sh 0.7.8              # 正式版
#   scripts/release.sh 0.7.9-beta5        # 预发布版（beta / alpha / rc 同理）
#   scripts/release.sh 0.7.7-fix1         # 指定补丁号
#   scripts/release.sh fix                # 自动取下一个补丁号（0.7.7 → 0.7.7-fix1）
#   scripts/release.sh fix --base 0.7.6   # 指定基准
#
# 选项：
#   --dry-run    只打印将要做的事，不改任何文件
#   --no-push    本地生成提交与 tag，但不推送
#
# 前置条件：
#   * 工作区干净；CHANGELOG 里补丁版小节需**先手工写好**（脚本会校验存在）
#   * 已安装 python3（用于改写 Cargo.toml / Cargo.lock / CHANGELOG，保证 UTF-8 无损）
#
set -euo pipefail

cd "$(git rev-parse --show-toplevel)"

DRY_RUN=0
PUSH=1
ARG=""
BASE=""

usage() {
    sed -n '3,25p' "$0" | sed 's/^# \{0,1\}//'
    exit "${1:-0}"
}

while [ $# -gt 0 ]; do
    case "$1" in
        -h|--help) usage 0 ;;
        --dry-run) DRY_RUN=1; shift ;;
        --no-push) PUSH=0; shift ;;
        --base) BASE="${2:-}"; shift 2 ;;
        -*) echo "未知选项: $1" >&2; usage 1 ;;
        *) ARG="$1"; shift ;;
    esac
done

[ -n "$ARG" ] || usage 1

run() {
    if [ "$DRY_RUN" -eq 1 ]; then
        echo "  [dry-run] $*"
    else
        "$@"
    fi
}

# ── 版本比较：五元组 (major minor patch stage num) ──────────────────────────
# 与 src/app.rs 的 parse_version **逐位一致**（stage：1=正式版 2=fix 3=alpha 4=beta 5=rc）。
#
# ⚠️ 这个顺序意味着 `0.7.9-beta1 > 0.7.9` —— 这是用户 2026/09/21 明确的约定：
# `-betaN` 是"正式版发出去之后、在它之上继续做出来的构建"（`-fixN` 的替代写法），
# 所以 `0.7.9-beta4` 的下一步是 **0.7.10**，而不是回头再发一次 `0.7.9`。
# 反过来写的话，应用的更新检查永远提示不出测试版（`latest <= current` 会判成"更旧"）。
ver_key() {
    local v="${1#v}" core="$1" stage=1 num=0 suffix=""
    if [ "${v#*-}" != "$v" ]; then
        core="${v%%-*}"
        suffix="${v#*-}"
        case "$suffix" in
            fix*)   stage=2; num="${suffix#fix}" ;;
            alpha*) stage=3; num="${suffix#alpha}" ;;
            beta*)  stage=4; num="${suffix#beta}" ;;
            rc*)    stage=5; num="${suffix#rc}" ;;
            *)      stage=1; num=0 ;;   # 认不出来的后缀按正式版容忍（同 app.rs）
        esac
        case "$num" in ''|*[!0-9]*) num=0 ;; esac
    fi
    local a b c
    IFS='.' read -r a b c <<<"$core"
    echo "${a:-0} ${b:-0} ${c:-0} $stage $num"
}

ver_gt() { # $1 > $2
    local -a x y
    read -r -a x <<<"$(ver_key "$1")"
    read -r -a y <<<"$(ver_key "$2")"
    local i
    for i in 0 1 2 3 4; do
        if [ "${x[$i]}" -gt "${y[$i]}" ] 2>/dev/null; then return 0; fi
        if [ "${x[$i]}" -lt "${y[$i]}" ] 2>/dev/null; then return 1; fi
    done
    return 1
}

current_version() {
    python3 - <<'PY'
import re
src = open('Cargo.toml', encoding='utf-8').read()
m = re.search(r'(?ms)^\[package\].*?^version\s*=\s*"([^"]+)"', src)
print(m.group(1) if m else '')
PY
}

# ── 解析目标版本 ────────────────────────────────────────────────────────────
if [ "$ARG" = "fix" ]; then
    [ -n "$BASE" ] || BASE="$(current_version)"
    # 基准若带后缀（-beta4 / -fix1），先剥掉：补丁号永远接在**正式版**后面。
    BASE_CORE="${BASE%%-*}"
    NEXT=1
    while git rev-parse -q --verify "refs/tags/v${BASE_CORE}-fix${NEXT}" >/dev/null; do
        NEXT=$((NEXT + 1))
    done
    VERSION="${BASE_CORE}-fix${NEXT}"
else
    VERSION="$ARG"
fi

case "$VERSION" in
    *-fix*) IS_FIX=1; CORE="${VERSION%%-fix*}"; SUFFIX="${VERSION##*-fix}" ;;
    *)      IS_FIX=0; CORE="$VERSION"; SUFFIX="" ;;
esac

if [ "$IS_FIX" -eq 1 ]; then
    case "$SUFFIX" in
        ''|*[!0-9]*) echo "错误：补丁号必须是数字，如 0.7.7-fix1（收到 $VERSION）" >&2; exit 1 ;;
    esac
fi
if ! [[ "$VERSION" =~ ^[0-9]+\.[0-9]+\.[0-9]+(-(fix|alpha|beta|rc)[0-9]+)?$ ]]; then
    echo "错误：版本号格式应为 X.Y.Z，或带 -fixN / -alphaN / -betaN / -rcN 后缀（收到 $VERSION）" >&2
    exit 1
fi

CUR="$(current_version)"
if [ -z "$CUR" ]; then echo "错误：读不到 Cargo.toml 里的 [package].version" >&2; exit 1; fi

echo "当前版本: $CUR"
case "$VERSION" in
    *-fix*)   KIND="  (补丁版)" ;;
    *-beta*)  KIND="  (测试版)" ;;
    *-alpha*) KIND="  (内测版)" ;;
    *-rc*)    KIND="  (候选版)" ;;
    *)        KIND="" ;;
esac
echo "目标版本: $VERSION$KIND"

if ! ver_gt "$VERSION" "$CUR"; then
    echo "错误：$VERSION 不大于当前版本 $CUR" >&2
    exit 1
fi

if git rev-parse -q --verify "refs/tags/v${VERSION}" >/dev/null; then
    echo "错误：tag v${VERSION} 已存在" >&2
    exit 1
fi

if ! git diff --quiet --exit-code || ! git diff --cached --quiet --exit-code; then
    echo "错误：工作区有未提交的改动，请先提交或 stash" >&2
    exit 1
fi

# ── CHANGELOG ───────────────────────────────────────────────────────────────
DATE="$(date +%Y-%m-%d)"
if [ "$IS_FIX" -eq 1 ]; then CHANGELOG_MODE=fix; else CHANGELOG_MODE=normal; fi

if [ "$DRY_RUN" -eq 1 ]; then
    echo "  [dry-run] python3 改写 CHANGELOG.md（模式: ${CHANGELOG_MODE}）"
else
    python3 - "$VERSION" "$DATE" "$CHANGELOG_MODE" "$CUR" <<'PY'
import re, sys

version, date, mode, current = sys.argv[1:5]
path = 'CHANGELOG.md'
src = open(path, encoding='utf-8').read()

if mode == 'fix':
    # 补丁版：小节必须**已经手工写好**（内容只有这一批修复，不该把 [Unreleased]
    # 里攒的下一个小版本内容提前发布出去）。
    if f'\n## [{version}]' not in src:
        sys.exit(
            f'错误：CHANGELOG.md 里没有 `## [{version}]` 小节。\n'
            f'补丁版请先在 `## [Unreleased]` 之上手工补一段：\n\n'
            f'## [{version}] - {date}\n\n### 修复 / Fixed\n\n- …\n'
        )
    print(f'CHANGELOG: 已存在 [{version}] 小节，跳过')
    sys.exit(0)

# 正式版：把 [Unreleased] 提升为这一版，再留一个新的空 [Unreleased]
new = re.sub(
    r'^## \[Unreleased\]$',
    f'## [Unreleased]\n\n## [{version}] - {date}',
    src,
    count=1,
    flags=re.M,
)
if new == src:
    sys.exit('错误：CHANGELOG.md 里找不到 `## [Unreleased]` 标题')
open(path, 'w', encoding='utf-8').write(new)
print(f'CHANGELOG: [Unreleased] → [{version}] - {date}')
PY
fi

# ── Cargo.toml / Cargo.lock ─────────────────────────────────────────────────
if [ "$DRY_RUN" -eq 1 ]; then
    echo "  [dry-run] Cargo.toml / Cargo.lock 版本号 → $VERSION"
else
    VERSION="$VERSION" python3 - <<'PY'
import os, re

version = os.environ['VERSION']

toml = open('Cargo.toml', encoding='utf-8').read()
new_toml, n = re.subn(
    r'(?ms)^(\[package\].*?^version\s*=\s*")[^"]+(")',
    lambda m: m.group(1) + version + m.group(2),
    toml,
    count=1,
)
assert n == 1, 'Cargo.toml 里的 [package].version 没改到'
open('Cargo.toml', 'w', encoding='utf-8').write(new_toml)

lock = open('Cargo.lock', encoding='utf-8').read()
new_lock, n = re.subn(
    r'(?ms)^(name\s*=\s*"rudder"\s*\nversion\s*=\s*")[^"]+(")',
    lambda m: m.group(1) + version + m.group(2),
    lock,
    count=1,
)
assert n == 1, 'Cargo.lock 里 rudder 的 version 没改到'
open('Cargo.lock', 'w', encoding='utf-8').write(new_lock)
print(f'Cargo.toml / Cargo.lock → {version}')
PY
fi

# ── 校验 ────────────────────────────────────────────────────────────────────
if [ "$DRY_RUN" -eq 1 ]; then
    echo "  [dry-run] cargo check --locked"
    echo "  [dry-run] 校验二进制 --version == rudder $VERSION"
else
    cargo check --locked
    actual="$(cargo run --locked --quiet -- --version)"
    if [ "$actual" != "rudder $VERSION" ]; then
        echo "错误：二进制 --version 输出 '$actual'，期望 'rudder $VERSION'" >&2
        exit 1
    fi
    cargo test
fi

# ── 提交 / 打 tag / 推送 ────────────────────────────────────────────────────
run git add Cargo.toml Cargo.lock CHANGELOG.md
run git commit -m "chore(release): $VERSION"
run git tag -a "v$VERSION" -m "Release v$VERSION"

if [ "$PUSH" -eq 1 ]; then
    run git push origin HEAD
    run git push origin "v$VERSION"
    echo ""
    echo "已推送 v$VERSION —— push tag 会自动触发 Release 工作流："
    echo "  gh run watch \$(gh run list --limit 1 --json databaseId -q '.[0].databaseId')"
else
    echo ""
    echo "已本地生成提交与 tag v$VERSION。推送："
    echo "  git push origin HEAD && git push origin v$VERSION"
fi
