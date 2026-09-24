fn main() {
    let manifest = r#"
<assembly xmlns="urn:schemas-microsoft-com:asm.v1" manifestVersion="1.0">
  <compatibility xmlns="urn:schemas-microsoft-com:compatibility.v1">
    <application>
      <supportedOS Id="{8e0f7a12-bfb3-4fe8-b9a5-48fd50a15a9a}"/>
    </application>
  </compatibility>
  <application xmlns="urn:schemas-microsoft-com:asm.v3">
    <windowsSettings>
      <dpiAware xmlns="http://schemas.microsoft.com/SMI/2005/WindowsSettings">true/PM</dpiAware>
      <dpiAwareness xmlns="http://schemas.microsoft.com/SMI/2016/WindowsSettings">PerMonitorV2</dpiAwareness>
    </windowsSettings>
  </application>
</assembly>
"#;

    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed=assets/icon.ico");
    println!("cargo:rerun-if-env-changed=TRAFFIC_MONITOR_DEV_BUILD");

    // 「本次构建是不是开发版」的唯一真值源：只有 scripts/package.ts 的 dev 打包路径注入
    // 该变量，常规与 CI 构建都不带。不要改成嗅探版本号后缀——package.ts 接受任意 tag
    // （dev / rc / foo 都会产出 x.y.z-<tag><ts>），后缀不构成可判定的约定。
    if std::env::var_os("TRAFFIC_MONITOR_DEV_BUILD").is_some() {
        println!("cargo:rustc-env=TRAFFIC_MONITOR_DEV_BUILD=1");
    }

    // /DELAYLOAD 让 winhttp/bcrypt 系列不进入标准导入表，主进程启动时不主动加载这些 DLL。
    // 配合 update 模块的 re-exec 子进程方案：更新下载、校验和更新专属交互在子进程中进行；
    // 子进程退出后由操作系统回收其 DLL 与内存。主进程仍保留窗口、托盘和基础错误提示 API。
    // 已验证：/DELAYLOAD 能穿透 windows-link 的 raw-dylib 链接机制。
    // /DELAYLOAD 是 MSVC link.exe 专属，MinGW (pc-windows-gnu) 不支持。
    let target = std::env::var("TARGET").unwrap_or_default();
    if target.contains("msvc") {
        println!("cargo:rustc-link-arg=/DELAYLOAD:winhttp.dll");
        println!("cargo:rustc-link-arg=/DELAYLOAD:bcrypt.dll");
        println!("cargo:rustc-link-arg=/DELAYLOAD:bcryptprimitives.dll");
        println!("cargo:rustc-link-lib=dylib=delayimp");
    }

    let target_os = std::env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();
    if target_os == "windows" {
        let mut res = winresource::WindowsResource::new();
        res.set_manifest(manifest);
        res.set_icon("assets/icon.ico");
        res.compile().unwrap();
    }
}
