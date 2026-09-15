fn main() {
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        println!("cargo:rerun-if-changed=assets/icon.ico");
        println!("cargo:rerun-if-changed=assets/app.rc");
        // Icon resource id 1 -- ui::tray::EMBEDDED_ICON_ID loads this, and the
        // shell uses the first icon in the binary for the .exe in Explorer.
        embed_resource::compile("assets/app.rc", embed_resource::NONE)
            .manifest_optional()
            .unwrap();
    }
}
