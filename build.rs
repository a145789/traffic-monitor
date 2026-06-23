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

    // /DELAYLOAD 让 winhttp/bcrypt 系列不进入标准导入表，进程启动时不会加载它们。
    // 配合 update.rs 的 re-exec 子进程方案：更新检查在子进程中进行，
    // 子进程退出后 winhttp + schannel + ncrypt + bcrypt 全部随进程释放，主进程稳态零开销。
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
