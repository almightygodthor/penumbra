use std::env;
use std::fs;
use std::path::PathBuf;

use winresource::WindowsResource;

fn windows_binary() {
    let mut res = WindowsResource::new();
    res.set_icon("res/windows/icon.ico");
    res.set_manifest_file("res/windows/manifest.xml");
    res.compile().unwrap();
}

fn main() {
    let target = env::var("TARGET").unwrap();
    let out_dir = PathBuf::from(env::var_os("OUT_DIR").expect("OUT_DIR is set by Cargo"));
    let embedded_zadig = out_dir.join("zadig.exe");

    // Stable release CI can provide the official Zadig executable through ZADIG_PATH.
    // Other builds get an empty placeholder so the app still compiles without it.
    println!("cargo:rerun-if-env-changed=ZADIG_PATH");
    if let Some(source) = env::var_os("ZADIG_PATH") {
        let source = PathBuf::from(source);
        if source.is_file() {
            fs::copy(source, &embedded_zadig).expect("Failed to embed Zadig executable");
        } else {
            panic!("ZADIG_PATH does not point to a file");
        }
    } else {
        fs::write(&embedded_zadig, []).expect("Failed to create Zadig placeholder");
    }

    println!("cargo:rerun-if-changed={}", embedded_zadig.display());

    if target.contains("windows") {
        windows_binary();
    }
}
