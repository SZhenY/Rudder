#Requires -Version 7.0
<#
.SYNOPSIS
    采样 rudder 进程内存，按阶段打点，输出 CSV 与「末 15 秒中位数」摘要。

.DESCRIPTION
    专用工作集（任务管理器「内存」列）走 Win32_PerfFormattedData_PerfProc_Process 的
    WorkingSetPrivate —— 属性名恒英文，不受中文 Windows 的计数器本地化影响。

    采样期间按 1-5 打点（同一键重复按会自动编号 #1/#2/#3，用来区分一次性预热与真泄漏）：
      1 = boot-idle       启动静置
      2 = sess-open       连上会话静置
      3 = sess-close      关掉标签后
      4 = settings-open   设置窗口打开
      5 = settings-close  设置窗口关闭后
      m = 自由标记（会暂停采样提示输入文字）
      q = 结束本轮

.EXAMPLE
    pwsh -NoProfile -File scripts/mem-probe.ps1
#>
[CmdletBinding()]
param(
    [string]$ProcessName = 'rudder',
    [int]$IntervalMs = 200,
    [int]$CimIntervalMs = 1000,
    [int]$SettleSeconds = 15,
    # >0 时无人值守跑这么久就收尾（-NoKeys 配合用，便于自动化冒烟）。
    [int]$DurationSeconds = 0,
    [string]$OutDir = $PSScriptRoot,
    [switch]$NoKeys
)

$ErrorActionPreference = 'Stop'
$inv = [System.Globalization.CultureInfo]::InvariantCulture

function Find-Target {
    param([string]$Name)
    $all = @(Get-Process -Name $Name -ErrorAction SilentlyContinue)
    if ($all.Count -eq 0) { throw "找不到进程 '$Name'，先把程序启动起来再运行本脚本。" }
    # --probe-renderer 子进程没有主窗口，排除掉。
    $main = @($all | Where-Object { $_.MainWindowHandle -ne 0 })
    if ($main.Count -eq 0) { throw "进程 '$Name' 存在但没有主窗口（可能还在启动）。稍后重试。" }
    if ($main.Count -gt 1) {
        $ids = ($main | ForEach-Object Id) -join ', '
        throw "有多个带主窗口的 '$Name'（PID: $ids），先只保留一个再测。"
    }
    return $main[0]
}

function Get-PerfSnapshot {
    param([int]$ProcId)
    $row = [ordered]@{ ws_private = $null; commit = $null; gpu_ded = $null; gpu_shared = $null }
    try {
        $cim = Get-CimInstance -ClassName Win32_PerfFormattedData_PerfProc_Process `
            -Filter "IDProcess=$ProcId" -ErrorAction Stop
        if ($cim) {
            $row.ws_private = [double]$cim.WorkingSetPrivate
            $row.commit = [double]$cim.PrivateBytes
        }
    } catch {
        # 性能计数器被禁用时会失败；WS 列留空，脚本继续跑。
    }
    try {
        $needle = "pid_" + $ProcId + "_*"
        $gpu = @(Get-CimInstance -ClassName Win32_PerfFormattedData_GPUPerformanceCounters_GPUProcessMemory `
                -Filter "Name like 'pid_$ProcId%'" -ErrorAction Stop |
                Where-Object { $_.Name -like $needle })
        if ($gpu.Count -gt 0) {
            $row.gpu_ded = [double]($gpu | Measure-Object -Property DedicatedUsage -Sum).Sum
            $row.gpu_shared = [double]($gpu | Measure-Object -Property SharedUsage -Sum).Sum
        }
    } catch {
        # 老 WDDM / 无独显时没有这个计数器类，GPU 列保持 NA。
    }
    return $row
}

function Format-Mb {
    param($Bytes)
    if ($null -eq $Bytes) { return 'NA' }
    return ([math]::Round([double]$Bytes / 1MB, 1)).ToString($inv)
}

function Get-Median {
    param([object[]]$Values)
    $v = @($Values | Where-Object { $null -ne $_ } | Sort-Object)
    if ($v.Count -eq 0) { return $null }
    $mid = [int][math]::Floor($v.Count / 2)
    if ($v.Count % 2 -eq 1) { return [double]$v[$mid] }
    return ([double]$v[$mid - 1] + [double]$v[$mid]) / 2
}

$proc = Find-Target -Name $ProcessName
$procId = $proc.Id
Write-Host "目标：$ProcessName (PID $procId)" -ForegroundColor Cyan

if (-not (Test-Path -LiteralPath $OutDir)) { New-Item -ItemType Directory -Path $OutDir | Out-Null }
$round = 1
$existing = @(Get-ChildItem -LiteralPath $OutDir -Filter 'rudder-mem-r*.csv' -ErrorAction SilentlyContinue)
if ($existing.Count -gt 0) {
    $nums = $existing | ForEach-Object { if ($_.BaseName -match '^rudder-mem-r(\d+)$') { [int]$Matches[1] } }
    if ($nums) { $round = ([int]($nums | Measure-Object -Maximum).Maximum) + 1 }
}
$csvPath = Join-Path $OutDir ("rudder-mem-r{0}.csv" -f $round)
$rows = [System.Collections.Generic.List[string]]::new()
$rows.Add('ts,elapsed_s,stage,ws_total_mb,ws_private_mb,commit_mb,threads,handles,gpu_dedicated_mb,gpu_shared_mb')

Write-Host @"
第 $round 轮 —— 阶段按键：
  1 boot-idle   2 sess-open   3 sess-close   4 settings-open   5 settings-close
  m 自由标记    q 结束本轮
每个阶段静置到读数稳定（建议 ≥15s）再按下一个键。
"@ -ForegroundColor DarkGray

$stage = 'boot-idle'
$counts = @{}
$counts[$stage] = 1
$current = '{0}#1' -f $stage
$sw = [System.Diagnostics.Stopwatch]::StartNew()
$lastCim = [System.Diagnostics.Stopwatch]::StartNew()
$lastMs = 0
$snap = $null
$quit = $false
$keysOk = -not $NoKeys
$interactive = -not [Console]::IsOutputRedirected
$nextCimAt = 0L

Write-Host "已开始打点：$current" -ForegroundColor Yellow
Write-Host ''

while (-not $quit) {
    if ($DurationSeconds -gt 0 -and $sw.Elapsed.TotalSeconds -ge $DurationSeconds) {
        Write-Host "`n到达 -DurationSeconds，结束采样。" -ForegroundColor Cyan
        break
    }
    $alive = Get-Process -Id $procId -ErrorAction SilentlyContinue
    if (-not $alive) { Write-Host "`n进程已退出，结束采样。" -ForegroundColor Red; break }

    if ($sw.ElapsedMilliseconds -ge $nextCimAt -or $null -eq $snap) {
        $snap = Get-PerfSnapshot -ProcId $procId
        $nextCimAt = $sw.ElapsedMilliseconds + $CimIntervalMs
    }

    $p = Get-Process -Id $procId -ErrorAction SilentlyContinue
    if ($p) {
        $line = '{0},{1:F1},{2},{3},{4},{5},{6},{7},{8},{9}' -f `
            (Get-Date -Format 'HH:mm:ss.fff'), `
            ($sw.ElapsedMilliseconds / 1000.0), `
            $current, `
            (Format-Mb $p.WorkingSet64), `
            (Format-Mb $snap.ws_private), `
            (Format-Mb $snap.commit), `
            $p.Threads.Count, `
            $p.HandleCount, `
            (Format-Mb $snap.gpu_ded), `
            (Format-Mb $snap.gpu_shared)
        $rows.Add($line)
        $msg = '  [{0}] WS专用 {1} MB  提交 {2} MB  线程 {3}  句柄 {4}  GPU {5} MB  ' -f `
            $current, (Format-Mb $snap.ws_private), (Format-Mb $snap.commit), $p.Threads.Count, $p.HandleCount, (Format-Mb $snap.gpu_ded)
        # \e[2K 整行清除，否则上一行较长时会留下残字。输出被重定向时不做原地重绘。
        if ($interactive) {
            Write-Host ("`r`e[2K" + $msg) -NoNewline -ForegroundColor Gray
        }
    }

    # 等待下一个采样点，期间轮询键盘，避免 Read-Host 把采样卡住。
    $deadline = $sw.ElapsedMilliseconds + $IntervalMs
    while ($sw.ElapsedMilliseconds -lt $deadline) {
        if ($keysOk) {
            try {
                if ([Console]::KeyAvailable) {
                    $key = [Console]::ReadKey($true)
                    $ch = [char]::ToLowerInvariant($key.KeyChar)
                    switch ($ch) {
                        '1' { $stage = 'boot-idle' }
                        '2' { $stage = 'sess-open' }
                        '3' { $stage = 'sess-close' }
                        '4' { $stage = 'settings-open' }
                        '5' { $stage = 'settings-close' }
                        'q' { $quit = $true }
                        'm' {
                            Write-Host ''
                            $note = Read-Host '标记文字'
                            if ([string]::IsNullOrWhiteSpace($note)) { $note = 'mark' }
                            $stage = $note.Trim()
                        }
                        default { }
                    }
                    if ($quit) { break }
                    if ($ch -in @('1', '2', '3', '4', '5', 'm')) {
                        $counts[$stage] = 1 + [int]($counts[$stage])
                        $current = '{0}#{1}' -f $stage, $counts[$stage]
                        Write-Host ("`n-> {0}" -f $current) -ForegroundColor Yellow
                    }                }
            } catch {
                $keysOk = $false
                Write-Host "`n[警告] 当前会话读不到键盘，改为 -NoKeys 模式（阶段固定为 $current）。" -ForegroundColor DarkYellow
            }
        }
        Start-Sleep -Milliseconds 20
    }
    $lastMs = $sw.ElapsedMilliseconds
}

Write-Host ''
Write-Host "采样结束，共 $($rows.Count - 1) 个样本。" -ForegroundColor Cyan
Set-Content -LiteralPath $csvPath -Value $rows -Encoding utf8
Write-Host "CSV: $csvPath"
Write-Host ''

# ---- 摘要：每个阶段实例取末 SettleSeconds 秒的中位数 ----
$parsed = $rows | Select-Object -Skip 1 | ForEach-Object {
    $f = $_ -split ','
    [pscustomobject]@{
        elapsed = [double]::Parse($f[1], $inv)
        stage   = $f[2]
        ws      = if ($f[4] -eq 'NA' -or $f[4] -eq '') { $null } else { [double]::Parse($f[4], $inv) }
        commit  = if ($f[5] -eq 'NA' -or $f[5] -eq '') { $null } else { [double]::Parse($f[5], $inv) }
        gpu     = if ($f[8] -eq 'NA' -or $f[8] -eq '') { $null } else { [double]::Parse($f[8], $inv) }
        threads = [int]$f[6]
    }
}

# 按**出现顺序**排（Group-Object 默认按名字排序，读时序会很别扭）。
$summary = foreach ($name in @($parsed | ForEach-Object { $_.stage } | Select-Object -Unique)) {
    $g = $parsed | Where-Object { $_.stage -eq $name } | Group-Object stage
    $last = ($g.Group | Measure-Object -Property elapsed -Maximum).Maximum
    $window = @($g.Group | Where-Object { $_.elapsed -ge ($last - $SettleSeconds) })
    [pscustomobject]@{
        Stage        = $name
        WS_MB        = [math]::Round((Get-Median ($window.ws | ForEach-Object { $_ })), 1)
        Commit_MB    = [math]::Round((Get-Median ($window.commit | ForEach-Object { $_ })), 1)
        GPU_MB       = [math]::Round((Get-Median ($window.gpu | ForEach-Object { $_ })), 1)
        Threads      = Get-Median ($window.threads | ForEach-Object { $_ })
        Samples      = $window.Count
    }
}

$base = $summary | Where-Object { $_.Stage -like 'boot-idle*' } | Select-Object -First 1
$summary = $summary | Select-Object *, @{n = 'Δ_vs_base'; e = {
        if ($base -and $null -ne $_.WS_MB -and $null -ne $base.WS_MB) { [math]::Round($_.WS_MB - $base.WS_MB, 1) } else { $null }
    } }

$summaryPath = [System.IO.Path]::ChangeExtension($csvPath, '.summary.txt')
$summary | Format-Table -AutoSize | Out-String -Width 200 | Set-Content -LiteralPath $summaryPath -Encoding utf8
Write-Host ''
$summary | Format-Table -AutoSize | Out-String -Width 200 | Write-Host
Write-Host ''

$roundsPath = Join-Path $OutDir 'rudder-mem-rounds.csv'
if (-not (Test-Path -LiteralPath $roundsPath)) {
    Set-Content -LiteralPath $roundsPath -Value 'round,stage,ws_mb,commit_mb,gpu_mb,threads' -Encoding utf8
}
foreach ($s in $summary) {
    Add-Content -LiteralPath $roundsPath -Encoding utf8 -Value ('{0},{1},{2},{3},{4},{5}' -f $round, $s.Stage, $s.WS_MB, $s.Commit_MB, $s.GPU_MB, $s.Threads)
}
Write-Host "摘要: $summaryPath"
Write-Host "跨轮汇总: $roundsPath（跑满 5 轮后对比同一 stage 的 ws_mb 是否单调上升）"
