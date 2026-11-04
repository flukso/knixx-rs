//! The public keys that the firmware and the bootloader take signed images
//! from, for their build scripts: written to $OUT_DIR/keys.rs as KEYS, by
//! key number.
//!
//! cargo xtask build passes them in KNIXX_KEYS, as `<number>:<hex>` items
//! separated by commas: the developer's key (number 0) and the keys of the
//! repository. Without it, as for cargo clippy, the keys of the repository
//! are used: keys/<number>-<name>.pub, each 32 bytes in hex.
use std::{env, fs, path::Path};

fn parse(item: &str) -> (u8, [u8; 32]) {
    let (id, hex) = item
        .trim()
        .split_once(':')
        .unwrap_or_else(|| panic!("KNIXX_KEYS item {item:?} is not <number>:<hex>"));
    (id.parse().expect("key number"), from_hex(hex))
}

fn from_hex(hex: &str) -> [u8; 32] {
    let hex = hex.trim();
    assert_eq!(hex.len(), 64, "a public key is 32 bytes in hex: {hex:?}");
    let mut k = [0u8; 32];
    for (i, b) in k.iter_mut().enumerate() {
        *b = u8::from_str_radix(&hex[2 * i..2 * i + 2], 16).expect("hex digits");
    }
    k
}

pub fn write(out: &Path) {
    println!("cargo:rerun-if-env-changed=KNIXX_KEYS");
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../keys");
    println!("cargo:rerun-if-changed={}", dir.display());
    let keys: Vec<(u8, [u8; 32])> = match env::var("KNIXX_KEYS") {
        Ok(v) if !v.trim().is_empty() => v.split(',').map(parse).collect(),
        _ => {
            let mut keys = Vec::new();
            for e in fs::read_dir(&dir).into_iter().flatten().flatten() {
                let name = e.file_name().to_string_lossy().into_owned();
                let Some(stem) = name.strip_suffix(".pub") else {
                    continue;
                };
                let Some((id, _)) = stem.split_once('-') else {
                    continue;
                };
                let Ok(id) = id.parse() else {
                    continue;
                };
                keys.push((id, from_hex(&fs::read_to_string(e.path()).unwrap())));
            }
            keys.sort();
            keys
        }
    };
    if keys.is_empty() {
        println!(
            "cargo:warning=no public keys: this build takes no signed image (build with cargo xtask build)"
        );
    }
    let mut src = String::from(
        "/// The public keys of signed images, by key number.\npub const KEYS: &[(u8, [u8; 32])] = &[\n",
    );
    for (id, k) in &keys {
        src.push_str(&format!("    ({id}, {k:?}),\n"));
    }
    src.push_str("];\n");
    fs::write(out.join("keys.rs"), src).unwrap();
}
