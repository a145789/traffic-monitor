# 版本管理

## 版本号位置

需要同步更新两个文件：

| 文件 | 字段 | 示例 |
|---|---|---|
| `Cargo.toml` | `version` | `version = "0.1.0"` |
| `installer.iss` | `AppVersion` | `AppVersion=0.1.0` |

## 版本号规范

采用语义化版本 `MAJOR.MINOR.PATCH`：

- **MAJOR** — 不兼容的 API 修改
- **MINOR** — 新增功能（向下兼容）
- **PATCH** — Bug 修复

示例：`0.1.0` → `0.1.1` → `0.2.0` → `1.0.0`

## 更新流程

### 1. 修改版本号

```toml
# Cargo.toml
version = "0.2.0"
```

```ini
# installer.iss
AppVersion=0.2.0
```

### 2. 编译

```powershell
cargo build --release
```

### 3. 打包安装程序

用 Inno Setup 打开 `installer.iss`，点击 Build > Compile。

输出文件：`Output\TrafficMonitor-Setup.exe`

### 4. 发布

建议重命名包含版本号：

```
TrafficMonitor-Setup-0.2.0.exe
```

## 自动化脚本（可选）

创建 `release.ps1`：

```powershell
param(
    [Parameter(Mandatory=$true)]
    [string]$Version
)

# 更新版本号
(Get-Content Cargo.toml) -replace 'version = ".*"', "version = `"$Version`"" | Set-Content Cargo.toml
(Get-Content installer.iss) -replace 'AppVersion=.*', "AppVersion=$Version" | Set-Content installer.iss

# 编译
cargo build --release

# 打包
& "C:\Program Files (x86)\Inno Setup 6\ISCC.exe" installer.iss

# 重命名
Rename-Item "Output\TrafficMonitor-Setup.exe" "TrafficMonitor-Setup-$Version.exe"

Write-Host "Release $Version complete!"
```

使用：

```powershell
.\release.ps1 -Version "0.2.0"
```

## 发布检查清单

- [ ] 更新 `Cargo.toml` 版本号
- [ ] 更新 `installer.iss` 版本号
- [ ] `cargo build --release` 无错误
- [ ] 运行 exe 测试功能正常
- [ ] Inno Setup 编译成功
- [ ] 安装包测试安装/卸载正常
- [ ] 重命名安装包包含版本号
