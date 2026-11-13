fn main() {
    // The AUBLst icon for the exe in Explorer; the window sets its own at runtime.
    println!("cargo:rerun-if-changed=assets/aublst.ico");
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        winresource::WindowsResource::new()
            .set_icon("assets/aublst.ico")
            .compile()
            .expect("embedding the exe icon needs rc.exe from the Windows SDK");
    }
}
