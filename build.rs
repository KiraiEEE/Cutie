#[path = "shared/icon_gen.rs"]
mod icon_gen;

fn main() {
    let out_dir = std::path::PathBuf::from(std::env::var("OUT_DIR").expect("OUT_DIR"));
    let ico = out_dir.join("cutie.ico");
    icon_gen::write_ico_file(&ico).expect("write icon");
    println!("cargo:rerun-if-changed=shared/icon_gen.rs");

    if cfg!(windows) {
        let mut res = winres::WindowsResource::new();
        res.set_icon(ico.to_string_lossy().as_ref());
        res.set("ProductName", "cutie");
        res.set("FileDescription", "cutie downloader");
        res.set("LegalCopyright", "cutie");
        res.set("ProductVersion", env!("CARGO_PKG_VERSION"));
        res.set("FileVersion", env!("CARGO_PKG_VERSION"));
        if let Err(e) = res.compile() {
            println!("cargo:warning=winres skipped: {e}");
        }
    }
}
