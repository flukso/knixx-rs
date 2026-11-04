//! The keys that sign firmware images (.kfw), and the public keys that the
//! firmware and the bootloader are built with.
//!
//! - **The developer's key**, number 0: a seed at
//!   ~/.config/knixx/dev-signing.key, made on first use. Every build on that
//!   machine signs with it and takes images signed with it.
//! - **The repository's keys**: keys/<number>-<name>.pub, the public halves
//!   of the release key (1) and its offline backup (2), whose private halves
//!   never enter the repository. The release workflow signs with
//!   $KNIXX_SIGNING_KEY (the seed in hex) as key $KNIXX_SIGNING_KEY_ID.

use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};

pub use knixx_image::sign::public_key;

/// A key that signs images: its 32-byte seed and its number.
#[derive(Clone)]
pub struct SigningKey {
    pub seed: [u8; 32],
    pub id: u8,
}

impl SigningKey {
    pub fn public(&self) -> [u8; 32] {
        public_key(&self.seed)
    }
}

/// The number of the developer's key.
pub const DEV_ID: u8 = 0;

/// Where the developer's key lives.
pub fn dev_key_path() -> Result<PathBuf, String> {
    let home = std::env::var_os("HOME").ok_or("no $HOME for the developer's signing key")?;
    Ok(Path::new(&home).join(".config/knixx/dev-signing.key"))
}

/// The developer's key, made on first use.
pub fn dev_key() -> Result<SigningKey, String> {
    let path = dev_key_path()?;
    if !path.exists() {
        let seed = random_seed()?;
        if let Some(dir) = path.parent() {
            fs::create_dir_all(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
        }
        write_private(&path, &format!("{}\n", hex(&seed)))?;
        eprintln!("made the developer's signing key {}", path.display());
    }
    let text = fs::read_to_string(&path).map_err(|e| format!("{}: {e}", path.display()))?;
    let seed = from_hex(text.trim()).map_err(|e| format!("{}: {e}", path.display()))?;
    Ok(SigningKey { seed, id: DEV_ID })
}

/// The key to sign with: $KNIXX_SIGNING_KEY if set, else the developer's.
pub fn signing_key() -> Result<SigningKey, String> {
    match std::env::var("KNIXX_SIGNING_KEY") {
        Ok(hexseed) if !hexseed.trim().is_empty() => {
            let seed = from_hex(hexseed.trim()).map_err(|e| format!("$KNIXX_SIGNING_KEY: {e}"))?;
            let id = std::env::var("KNIXX_SIGNING_KEY_ID")
                .ok()
                .map(|s| s.trim().parse::<u8>())
                .transpose()
                .map_err(|e| format!("$KNIXX_SIGNING_KEY_ID: {e}"))?
                .unwrap_or(1);
            Ok(SigningKey { seed, id })
        }
        _ => dev_key(),
    }
}

/// The public keys in root/keys: keys/<number>-<name>.pub.
pub fn repo_keys(root: &Path) -> Result<Vec<(u8, [u8; 32])>, String> {
    let dir = root.join("keys");
    let mut keys = Vec::new();
    let Ok(entries) = fs::read_dir(&dir) else {
        return Ok(keys);
    };
    for e in entries.flatten() {
        let name = e.file_name().to_string_lossy().into_owned();
        let Some(stem) = name.strip_suffix(".pub") else {
            continue;
        };
        let Some(id) = stem.split_once('-').and_then(|(id, _)| id.parse::<u8>().ok()) else {
            continue;
        };
        let text = fs::read_to_string(e.path()).map_err(|e| format!("{name}: {e}"))?;
        keys.push((id, from_hex(text.trim()).map_err(|e| format!("{name}: {e}"))?));
    }
    keys.sort();
    Ok(keys)
}

/// The keys a build takes, as KNIXX_KEYS for the build scripts: the
/// repository's, and the signing key's public half unless it is one of
/// them.
pub fn keys_env(root: &Path, signer: &SigningKey) -> Result<String, String> {
    let mut keys = repo_keys(root)?;
    if !keys.iter().any(|(id, _)| *id == signer.id) {
        keys.push((signer.id, signer.public()));
    } else if !keys.contains(&(signer.id, signer.public())) {
        return Err(format!("the signing key does not match keys/{}-*.pub", signer.id));
    }
    keys.sort();
    Ok(keys
        .iter()
        .map(|(id, k)| format!("{id}:{}", hex(k)))
        .collect::<Vec<_>>()
        .join(","))
}

/// A new seed from the operating system's random source.
pub fn random_seed() -> Result<[u8; 32], String> {
    let mut seed = [0u8; 32];
    fs::File::open("/dev/urandom")
        .and_then(|mut f| f.read_exact(&mut seed))
        .map_err(|e| format!("/dev/urandom: {e}"))?;
    Ok(seed)
}

/// Writes a private key readable by its owner only.
pub fn write_private(path: &Path, text: &str) -> Result<(), String> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    let mut f = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)
        .map_err(|e| format!("{}: {e}", path.display()))?;
    f.write_all(text.as_bytes())
        .map_err(|e| format!("{}: {e}", path.display()))
}

pub fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

pub fn from_hex(s: &str) -> Result<[u8; 32], String> {
    if s.len() != 64 {
        return Err(format!("a key is 32 bytes in hex, not {} characters", s.len()));
    }
    let mut k = [0u8; 32];
    for (i, b) in k.iter_mut().enumerate() {
        *b = u8::from_str_radix(&s[2 * i..2 * i + 2], 16).map_err(|e| e.to_string())?;
    }
    Ok(k)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hex_round_trip() {
        let k = [0xA5; 32];
        assert_eq!(from_hex(&hex(&k)).unwrap(), k);
        assert!(from_hex("abc").is_err());
    }

    #[test]
    fn keys_env_adds_the_signer() {
        let root = std::env::temp_dir().join(format!("knixx-keys-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(root.join("keys")).unwrap();
        let release = SigningKey { seed: [1; 32], id: 1 };
        fs::write(root.join("keys/1-release.pub"), hex(&release.public())).unwrap();
        let dev = SigningKey { seed: [2; 32], id: 0 };
        let env = keys_env(&root, &dev).unwrap();
        assert_eq!(
            env,
            format!("0:{},1:{}", hex(&dev.public()), hex(&release.public()))
        );
        // Signing as the release key adds nothing, and a wrong seed fails.
        assert_eq!(
            keys_env(&root, &release).unwrap(),
            format!("1:{}", hex(&release.public()))
        );
        let wrong = SigningKey { seed: [3; 32], id: 1 };
        assert!(keys_env(&root, &wrong).is_err());
        fs::remove_dir_all(&root).unwrap();
    }
}
