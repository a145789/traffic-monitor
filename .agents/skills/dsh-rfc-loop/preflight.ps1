# 本文件只是转发壳：预检逻辑的唯一真值在 preflight.ts（Bun 实现）。
#
# 规范入口：
#     bun .agents/skills/dsh-rfc-loop/preflight.ts
#
# 保留这个壳只是为了让「顺手敲 pwsh -File …preflight.ps1」仍然可用。
# 不要在 .ps1 里复刻逻辑，两份实现必然会漂移。

$ts = Join-Path $PSScriptRoot 'preflight.ts'

if (-not (Get-Command bun -ErrorAction SilentlyContinue)) {
    Write-Output 'PREFLIGHT: STOP'
    Write-Output 'STOP: bun not found on PATH (needed to run preflight.ts)'
    exit 2
}

& bun $ts @args
exit $LASTEXITCODE
