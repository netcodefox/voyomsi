//! On Windows, the application icon goes into `voyomsi-launcher.exe`.

fn main() {
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("windows") {
        return;
    }
    println!("cargo:rerun-if-changed=../../assets/icons/app/voyomsi.ico");
    let mut res = winresource::WindowsResource::new();
    res.set_icon("../../assets/icons/app/voyomsi.ico").set("ProductName", "VoyOMSI").set("FileDescription", "VoyOMSI launcher tools");
    if let Err(e) = res.compile() {
        println!("cargo:warning=no icon in the executable: {e}");
    }
}
