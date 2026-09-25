# 安装器兜底强杀筛选器：installer.iss 的 ForceKillRemnant 在优雅退出超时后调用。
#
# 本文件必须以 UTF-8 BOM 保存：安装器用 Windows PowerShell 5.1（powershell.exe）执行，
# 无 BOM 时它按 ANSI 代码页解码本文件，中文会变成乱码（且可能吃掉引号）。
#
# 安装器以管理员身份运行，taskkill /F /PID 没有路径与会话边界，所以这里只终止
# 同时满足三个必要条件的进程，任一条件取不到数据一律跳过（保守方向 = 少杀）：
#   1. 可执行文件的完整路径等于 -Expected，即本次正在升级的那个安装目录；
#   2. 进程所在会话等于安装器所在会话，挡掉异会话里同路径的实例；
#   3. 命令行取得到且不含 --check-update——同目录的更新协调者 re-exec 必须保留。
# 与旧实现（按映像名 + 命令行排除）相比，误伤面从「全机器同名进程」收窄到
# 「本次升级目标的同会话实例」。本脚本只做「少杀」不做跨会话清理：异会话残留退回
# 安装器原生的「文件被占用」提示，属预期行为。
# 刻意不启用 Set-StrictMode：缺属性在非严格模式下取 $null，会自然落入「不杀」，
# 方向安全；严格模式会把可恢复的取数失败变成脚本级异常。
param(
    [Parameter(Mandatory = $true)][string]$Expected,
    [object[]]$Processes,
    [switch]$DryRun
)

# 身份约束缺失时一律不杀：路径比较是唯一把「同名进程」钉到本次升级目标的条件。
if ([string]::IsNullOrEmpty($Expected)) {
    Write-Output '筛选器：未提供 -Expected，不终止任何进程。'
    exit 0
}

# 安装器所在会话，供候选进程比较。
$currentSession = (Get-Process -Id $PID).SessionId

# 候选来源：-Processes 由调用方喂入（验收/演练用构造记录），缺省才枚举本机进程。
$candidates = @()
if ($PSBoundParameters.ContainsKey('Processes')) {
    $candidates = @($Processes)
}
else {
    try {
        $candidates = @(Get-CimInstance Win32_Process -ErrorAction Stop)
    }
    catch {
        # CIM 不可用（WMI 损坏、策略限制）时静默放弃：少杀是安全方向，
        # 残留进程交由安装器原生文件占用提示处理。
        Write-Output '筛选器：枚举进程失败，不终止任何进程。'
        exit 0
    }
}

# 三个必要条件全部满足才是目标；条件取不到数据即不匹配（$null 比较自然为假）。
$targets = @($candidates | Where-Object {
        ($_.ExecutablePath -eq $Expected) -and
        ($_.SessionId -eq $currentSession) -and
        ($null -ne $_.CommandLine) -and
        ($_.CommandLine -notlike '*--check-update*')
    })

if ($targets.Count -eq 0) {
    Write-Output '筛选器：没有符合条件的残留进程。'
    exit 0
}

# TOCTOU 取舍：路径与会话校验发生在 CIM 快照时刻，taskkill 发生在之后，存在毫秒级
# PID 复用窗口。目标已收窄到单一路径，彻底闭合需要在脚本里 OpenProcess +
# TerminateProcess（把句柄校验与终止合成一步），代价与复杂度不成比例，本次不做。
foreach ($target in $targets) {
    if ($DryRun) {
        Write-Output ('[演练] 将终止 PID {0}' -f $target.ProcessId)
    }
    else {
        taskkill /F /PID $target.ProcessId | Out-Null
    }
}
