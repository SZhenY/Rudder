Set-Location 'D:\Workbuddy\Rudder\Rudder'
git add -A
$msg = @"
v0.6.10-beta4: 修复 53 号色块回归,系统字体可选,清理编译警告

修复:
- SGR 拦截器误伤 256 色索引:48;5;53m 的 53 被误判为上划线参数剥离,导致 53 号色块背景透明(纯黑/纯白背景下显示为主题背景色);同样误伤 38;5;53 前景与 48;5;21/38;5;21;拦截器现跳过 38;5;N/48;5;N 的索引参数,独立 53m 上划线不受影响
- 移除暗色主题 256 色背景提亮,恢复精确 xterm 标准色板(53 号 = 95,0,95 暗品红,与 Windows Terminal/PowerShell 一致)

新特性:
- 设置面板字体下拉框新增系统等宽字体(fontdb 枚举),系统字体列在最后
- 字体列表改为分组标题格式(内嵌字体/外置字体/系统字体 + 缩进家族名),标题行不可选,选择后仅保存纯家族名

清理:
- 修复 3 个编译警告:close 回调重命名为 request-close(避免遮蔽内置函数)、无用 mut、死代码标注

测试:169 项全部通过
"@
$tmp = Join-Path $env:TEMP 'commit_msg_beta4.txt'
[System.IO.File]::WriteAllText($tmp, $msg, (New-Object System.Text.UTF8Encoding($false)))
git commit -F $tmp
$code = $LASTEXITCODE
Write-Output "COMMIT_EXIT=$code"
git log --oneline -2
exit $code
