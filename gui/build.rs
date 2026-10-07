//! Embeds the icon, version information and the application manifest into
//! the Windows executable.

fn main() {
    println!("cargo:rerun-if-changed=assets/wdfr.ico");
    println!("cargo:rerun-if-changed=assets/wdfr.manifest");
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("windows") {
        return;
    }
    let mut res = winresource::WindowsResource::new();
    res.set_icon("assets/wdfr.ico")
        .set_manifest_file("assets/wdfr.manifest")
        .set("ProductName", "Deleted Files Recovery")
        .set("FileDescription", "Deleted Files Recovery")
        .set("CompanyName", "Bashar Salmo")
        .set("LegalCopyright", "© 2026 Bashar Salmo. Powered by Bashar Salmo.")
        .set("OriginalFilename", "wdfr-gui.exe");
    // Cross-compiling without a resource compiler still produces a working
    // (icon-less) binary; release builds on Windows always have one.
    if let Err(e) = res.compile() {
        println!("cargo:warning=Windows resources not embedded: {e}");
    }
}
