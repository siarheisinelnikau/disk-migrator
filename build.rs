fn main() {
    println!("cargo:rerun-if-changed=assets/app.ico");
    println!("cargo:rerun-if-changed=native/vss_bridge.cpp");
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        cc::Build::new()
            .cpp(true)
            .file("native/vss_bridge.cpp")
            .flag_if_supported("/std:c++17")
            .flag_if_supported("/EHsc")
            .define("UNICODE", None)
            .define("_UNICODE", None)
            .compile("disk_migrator_vss");
        for library in ["vssapi", "ole32", "oleaut32"] {
            println!("cargo:rustc-link-lib={library}");
        }
        let mut resource = winresource::WindowsResource::new();
        resource.set_icon("assets/app.ico");
        resource.set("ProductName", "Disk Migrator");
        resource.set("FileDescription", "Disk Migrator image migration wizard");
        resource.set_manifest(r#"<assembly xmlns="urn:schemas-microsoft-com:asm.v1" manifestVersion="1.0"><trustInfo xmlns="urn:schemas-microsoft-com:asm.v3"><security><requestedPrivileges><requestedExecutionLevel level="asInvoker" uiAccess="false"/></requestedPrivileges></security></trustInfo><application xmlns="urn:schemas-microsoft-com:asm.v3"><windowsSettings><dpiAwareness xmlns="http://schemas.microsoft.com/SMI/2016/WindowsSettings">PerMonitorV2</dpiAwareness></windowsSettings></application></assembly>"#);
        resource
            .compile()
            .expect("compile application icon resources");
    }
}
