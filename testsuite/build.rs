use std::env;
use std::fs;
use std::path::PathBuf;

fn main() {
    let out_dir = PathBuf::from(env::var("OUT_DIR").unwrap());

    // Copy memory.x to OUT_DIR
    fs::copy("memory.x", out_dir.join("memory.x")).unwrap();

    // Tell rustc where to find the linker script
    println!("cargo:rustc-link-search={}", out_dir.display());

    // Rebuild if memory.x changes
    println!("cargo:rerun-if-changed=memory.x");
}
