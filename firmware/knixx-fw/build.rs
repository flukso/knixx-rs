//! Puts memory.x where the linker finds it, and passes on the version that
//! cargo xtask build stamps the firmware with.
use std::{env, fs, path::PathBuf};

fn main() {
    let out = PathBuf::from(env::var_os("OUT_DIR").unwrap());
    fs::copy("memory.x", out.join("memory.x")).unwrap();
    println!("cargo:rustc-link-search={}", out.display());
    println!("cargo:rerun-if-changed=memory.x");
    for var in ["KNIXX_COMMIT", "KNIXX_BUILD_DATE"] {
        let v = env::var(var).unwrap_or_else(|_| "unknown".into());
        println!("cargo:rustc-env={var}={v}");
        println!("cargo:rerun-if-env-changed={var}");
    }
}
