<#
.SYNOPSIS
    Rudder 发版脚本（Windows）。macOS / Linux 用 scripts/release.sh，两者语义一致。

.DESCRIPTION
    版本形态（见 docs/release-process.md）：
      X.Y.Z              正式版（把 CHANGELOG 的 [Unreleased] 提升为这一版）
      X.Y.Z-fixN         补丁版：CHANGELOG 小节必须**先手工写好**（脚本会校验存在）
      X.Y.Z-alphaN / -betaN / -rcN
                         预发布版：同样把 [Unreleased] 提升为这一版

    版本比较与 src/app.rs 的 parse_version **逐位一致**（stage：1=正式版 2=fix 3=alpha
    4=beta 5=rc），因此 `0.7.9-beta1 > 0.7.9` —— 这是约定：`-betaN` 是"正式版发出去之后、
    在它之上继续做出来的构建"，所以 beta4 之后该发 0.7.10。理由详见 scripts/release.sh。

.EXAMPLE
    ./scripts/release.ps1 -Tag v0.7.9-beta5 -DryRun
    ./scripts/release.ps1 -Tag 0.7.9-beta5 -Push
#>
param(
    [Parameter(Mandatory = $true, Position = 0)]
    [ValidatePattern('^v?\d+\.\d+\.\d+(-(fix|alpha|beta|rc)\d+)?$')]
    [string] $Tag,

    [switch] $Push,
    [switch] $DryRun
)

$ErrorActionPreference = "Stop"

function Run-Git {
    param([string[]] $GitArgs)

    if ($DryRun) {
        Write-Host "git $($GitArgs -join ' ')"
        return
    }

    & git @GitArgs
    if ($LASTEXITCODE -ne 0) {
        throw "git $($GitArgs -join ' ') failed"
    }
}

function Run-Cargo {
    param([string[]] $CargoArgs)

    if ($DryRun) {
        Write-Host "cargo $($CargoArgs -join ' ')"
        return
    }

    & cargo @CargoArgs
    if ($LASTEXITCODE -ne 0) {
        throw "cargo $($CargoArgs -join ' ') failed"
    }
}

function Run-CheckedOutput {
    param(
        [string] $Expected,
        [Parameter(ValueFromRemainingArguments = $true)][string[]] $Command
    )

    if ($DryRun) {
        Write-Host "$($Command -join ' ')"
        return
    }

    $rawOutput = & $Command[0] @($Command | Select-Object -Skip 1)
    if ($LASTEXITCODE -ne 0) {
        throw "$($Command -join ' ') failed"
    }
    $output = ($rawOutput | Out-String).Trim()
    if ($output -ne $Expected) {
        throw "Expected '$Expected' but got '$output'."
    }
}

# ── 版本比较：五元组 (major minor patch stage num) ──────────────────────────
# 与 src/app.rs 的 parse_version 逐位一致；stage 的顺序含义见文件头。
function Get-VersionKey {
    param([string] $Value)

    $v = $Value.Trim().TrimStart('v')
    $core = $v
    $stage = 1
    $num = 0

    $dash = $v.IndexOf('-')
    if ($dash -ge 0) {
        $core = $v.Substring(0, $dash)
        $suffix = $v.Substring($dash + 1)
        $numText = ''
        if ($suffix -like 'fix*') { $stage = 2; $numText = $suffix.Substring(3) }
        elseif ($suffix -like 'alpha*') { $stage = 3; $numText = $suffix.Substring(5) }
        elseif ($suffix -like 'beta*') { $stage = 4; $numText = $suffix.Substring(4) }
        elseif ($suffix -like 'rc*') { $stage = 5; $numText = $suffix.Substring(2) }
        else { $stage = 1; $numText = '' }   # 认不出来的后缀按正式版容忍（同 app.rs）

        $parsed = 0
        if ([int]::TryParse($numText, [ref] $parsed)) { $num = $parsed }
    }

    $parts = $core.Split('.')
    $major = 0
    $minor = 0
    $patch = 0
    if ($parts.Length -gt 0) { [void] [int]::TryParse($parts[0], [ref] $major) }
    if ($parts.Length -gt 1) { [void] [int]::TryParse($parts[1], [ref] $minor) }
    if ($parts.Length -gt 2) { [void] [int]::TryParse($parts[2], [ref] $patch) }

    # 逗号前缀：不加的话 PowerShell 会把数组展开成多个返回值
    return , @($major, $minor, $patch, $stage, $num)
}

function Test-VersionGreater {
    param([string] $A, [string] $B)

    $x = Get-VersionKey $A
    $y = Get-VersionKey $B
    for ($i = 0; $i -lt 5; $i++) {
        if ($x[$i] -gt $y[$i]) { return $true }
        if ($x[$i] -lt $y[$i]) { return $false }
    }
    return $false
}

$repoRoot = (& git rev-parse --show-toplevel).Trim()
if ($LASTEXITCODE -ne 0 -or [string]::IsNullOrWhiteSpace($repoRoot)) {
    throw "This script must be run inside a git repository."
}

Set-Location $repoRoot

& git diff --quiet --exit-code
if ($LASTEXITCODE -ne 0) {
    throw "Tracked files have unstaged changes. Commit or stash them before releasing."
}

& git diff --cached --quiet --exit-code
if ($LASTEXITCODE -ne 0) {
    throw "Tracked files have staged changes. Commit or stash them before releasing."
}

$version = $Tag.Trim()
if ($version.StartsWith('v')) { $version = $version.Substring(1) }
$tag = "v$version"

$existingTag = (& git tag --list $tag)
if ($existingTag) {
    throw "Tag '$tag' already exists."
}

# 必须比当前版本更新 —— 与 release.sh 同一条检查（顺序语义见文件头）。
$utf8NoBom = New-Object System.Text.UTF8Encoding($false)
$cargoTomlCurrent = [System.IO.File]::ReadAllText((Join-Path $repoRoot "Cargo.toml"), $utf8NoBom)
$currentMatch = [regex]::Match(
    $cargoTomlCurrent, '(?ms)^\[package\].*?^version\s*=\s*"([^"]+)"')
if (-not $currentMatch.Success) {
    throw "Could not read [package].version from Cargo.toml."
}
$currentVersion = $currentMatch.Groups[1].Value
Write-Host "当前版本: $currentVersion"
Write-Host "目标版本: $version"

if (-not (Test-VersionGreater $version $currentVersion)) {
    throw "$version 不大于当前版本 $currentVersion（beta / fix 的先后见文件头的约定说明）"
}
$cargoTomlPath = Join-Path $repoRoot "Cargo.toml"
$cargoLockPath = Join-Path $repoRoot "Cargo.lock"

# ── CHANGELOG ───────────────────────────────────────────────────────────────
$changelogPath = Join-Path $repoRoot "CHANGELOG.md"

if ($DryRun) {
    Write-Host "Would update CHANGELOG.md for $version"
} else {
    $changelog = [System.IO.File]::ReadAllText($changelogPath, $utf8NoBom)

    if ($version -like '*-fix*') {
        # 补丁版：小节必须**已经手工写好**（内容只有这一批修复，不该把 [Unreleased]
        # 里攒的下一个小版本内容提前发布出去）。
        if ($changelog -notmatch "(?m)^## \[$([regex]::Escape($version))\]") {
            throw ("CHANGELOG.md 里没有 `## [$version]` 小节。补丁版请先在 [Unreleased] " +
                   "之上手工补一段，再重跑本脚本。")
        }
        Write-Host "CHANGELOG: 已存在 [$version] 小节，跳过"
    } else {
        # 正式版 / 预发布版：把 [Unreleased] 提升为这一版，再留一个新的空 [Unreleased]。
        $date = Get-Date -Format 'yyyy-MM-dd'
        $newChangelog = [regex]::Replace(
            $changelog,
            '(?m)^## \[Unreleased\]$',
            "## [Unreleased]`n`n## [$version] - $date",
            1
        )
        if ($newChangelog -eq $changelog) {
            throw "CHANGELOG.md 里找不到 `## [Unreleased]` 标题。"
        }
        [System.IO.File]::WriteAllText($changelogPath, $newChangelog, $utf8NoBom)
        Write-Host "CHANGELOG: [Unreleased] → [$version] - $date"
    }
}

$cargoToml = Get-Content -LiteralPath $cargoTomlPath -Raw
$newCargoToml = [regex]::Replace(
    $cargoToml,
    '(?ms)^(\[package\]\s+.*?^version\s*=\s*")[^"]+(")',
    "`${1}$version`${2}",
    1
)
if ($newCargoToml -eq $cargoToml) {
    throw "Could not update [package].version in Cargo.toml."
}

$cargoLock = Get-Content -LiteralPath $cargoLockPath -Raw
$newCargoLock = [regex]::Replace(
    $cargoLock,
    '(?ms)^(name\s*=\s*"rudder"\s*)(\r?\n)(version\s*=\s*")[^"]+(")',
    "`${1}`${2}`${3}$version`${4}",
    1
)
if ($newCargoLock -eq $cargoLock) {
    throw "Could not update rudder version in Cargo.lock."
}

if ($DryRun) {
    Write-Host "Would set Cargo.toml and Cargo.lock version to $version."
} else {
    # Windows PowerShell 5 uses the active ANSI code page for Set-Content by
    # default, which corrupts non-ASCII comments and makes Cargo reject the
    # manifests as invalid UTF-8. Write explicit UTF-8 without a BOM instead.
    $utf8NoBom = New-Object System.Text.UTF8Encoding($false)
    [System.IO.File]::WriteAllText($cargoTomlPath, $newCargoToml, $utf8NoBom)
    [System.IO.File]::WriteAllText($cargoLockPath, $newCargoLock, $utf8NoBom)
}

Run-Cargo -CargoArgs @("check", "--locked")
Run-CheckedOutput -Expected "rudder $version" -Command @(
    "cargo", "run", "--locked", "--", "--version"
)
Run-Cargo -CargoArgs @("test")

Run-Git -GitArgs @("add", "Cargo.toml", "Cargo.lock", "CHANGELOG.md")
Run-Git -GitArgs @("commit", "-m", "chore(release): $version")
Run-Git -GitArgs @("tag", "-a", $tag, "-m", "Release $tag")

if ($Push) {
    Run-Git -GitArgs @("push", "origin", "HEAD")
    Run-Git -GitArgs @("push", "origin", $tag)
    Write-Host "Released $tag and pushed branch + tag."
} else {
    Write-Host "Created release commit and tag $tag."
    Write-Host "Push with: git push origin HEAD && git push origin $tag"
}
