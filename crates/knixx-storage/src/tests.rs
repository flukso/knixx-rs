use std::string::{String, ToString};
use std::vec;
use std::vec::Vec;

use super::*;

/// A NOR flash in RAM: erased bytes are 0xFF and writes can only clear
/// bits, so a write over data that was not erased fails the test.
struct RamFlash {
    data: Vec<u8>,
}

impl RamFlash {
    fn blank() -> RamFlash {
        RamFlash {
            data: vec![0xFF; SECTOR_SIZE * SECTOR_COUNT],
        }
    }

    /// Returns a flash with the sectors of a sparse image: records of a
    /// little-endian u32 sector number and the sector's bytes.
    fn from_sparse(img: &[u8]) -> RamFlash {
        let mut f = RamFlash::blank();
        for rec in img.chunks(4 + SECTOR_SIZE) {
            let n = u32::from_le_bytes(rec[..4].try_into().unwrap()) as usize;
            f.data[n * SECTOR_SIZE..(n + 1) * SECTOR_SIZE].copy_from_slice(&rec[4..]);
        }
        f
    }

    /// Returns the sectors that are not blank, as a sparse image.
    fn to_sparse(&self) -> Vec<u8> {
        let mut img = Vec::new();
        for (n, s) in self.data.chunks(SECTOR_SIZE).enumerate() {
            if s.iter().any(|&b| b != 0xFF) {
                img.extend_from_slice(&(n as u32).to_le_bytes());
                img.extend_from_slice(s);
            }
        }
        img
    }
}

impl BlockDevice for RamFlash {
    type Error = ();

    fn read(&mut self, off: u32, buf: &mut [u8]) -> Result<(), ()> {
        let off = off as usize;
        buf.copy_from_slice(&self.data[off..off + buf.len()]);
        Ok(())
    }

    fn write(&mut self, off: u32, data: &[u8]) -> Result<(), ()> {
        assert_eq!(off as usize % PAGE_SIZE, 0, "unaligned write at {off:#x}");
        assert_eq!(data.len() % PAGE_SIZE, 0, "partial page written at {off:#x}");
        for (i, &b) in data.iter().enumerate() {
            let cell = &mut self.data[off as usize + i];
            assert_eq!(
                b & !*cell,
                0,
                "write over unerased data at {:#x}",
                off as usize + i
            );
            *cell &= b;
        }
        Ok(())
    }

    fn erase_sector(&mut self, off: u32) -> Result<(), ()> {
        assert_eq!(off as usize % SECTOR_SIZE, 0, "unaligned erase at {off:#x}");
        self.data[off as usize..off as usize + SECTOR_SIZE].fill(0xFF);
        Ok(())
    }
}

fn load_str(flash: &mut RamFlash) -> (Result<String, Error>, String) {
    let mut buf = [0u8; config::MAX_FILE];
    let mut log = String::new();
    let r = load(flash, &mut buf, &mut log).map(|n| String::from_utf8(buf[..n].to_vec()).unwrap());
    (r, log)
}

/// The configuration the Go images hold: the default with another
/// individual address and switch A on two groups.
const GO_CONFIG: &str = "[device]\nindividual = 7.8.9\n";

#[test]
fn blank_flash_is_formatted() {
    let mut flash = RamFlash::blank();
    let (r, log) = load_str(&mut flash);
    assert_eq!(r.unwrap().as_bytes(), config::DEFAULT);
    assert_eq!(
        log,
        "no littlefs found, formatting flash\nwrote default /knixx.ini\n"
    );
    assert!(has_superblock(&mut flash));

    let (r, log) = load_str(&mut flash);
    assert_eq!(r.unwrap().as_bytes(), config::DEFAULT);
    assert_eq!(log, "mounted littlefs\n");
}

#[test]
fn save_and_load() {
    let mut flash = RamFlash::blank();
    load_str(&mut flash).0.unwrap();
    save(&mut flash, b"[device]\nindividual = 1.1.1\n").unwrap();
    assert_eq!(load_str(&mut flash).0.unwrap(), "[device]\nindividual = 1.1.1\n");
    // A shorter file replaces it whole.
    save(&mut flash, b"x\n").unwrap();
    assert_eq!(load_str(&mut flash).0.unwrap(), "x\n");
}

#[test]
fn missing_file_is_written() {
    let mut flash = RamFlash::blank();
    format(&mut flash).unwrap();
    let (r, log) = load_str(&mut flash);
    assert_eq!(r.unwrap().as_bytes(), config::DEFAULT);
    assert_eq!(log, "mounted littlefs\nwrote default /knixx.ini\n");
}

#[test]
fn too_large() {
    let mut flash = RamFlash::blank();
    load_str(&mut flash).0.unwrap();
    save(&mut flash, &[b'x'; config::MAX_FILE]).unwrap();
    let r = load_str(&mut flash).0;
    assert_eq!(r, Err(Error::TooLarge));
    assert_eq!(r.unwrap_err().to_string(), "/knixx.ini: larger than 1024 bytes");
}

/// A littlefs that does not mount is not formatted: its superblock stays,
/// and the error says so.
#[test]
fn corrupt_littlefs_is_kept() {
    let mut flash = RamFlash::blank();
    load_str(&mut flash).0.unwrap();
    // Corrupt the metadata after the superblock entry in both blocks of
    // the pair, keeping the magic.
    for block in 0..2 {
        let at = block * SECTOR_SIZE + 24;
        flash.data[at..at + 64].fill(0x00);
    }
    let before = flash.data.clone();
    let (r, log) = load_str(&mut flash);
    let e = r.unwrap_err();
    assert!(matches!(e, Error::Mount(_, true)), "{e:?}");
    assert!(e.to_string().ends_with("; the flash is not formatted"), "{e}");
    assert_eq!(log, "");
    assert!(flash.data == before, "the flash changed");
}

/// Reads an image made by the Go firmware's littlefs (tinyfs) with the
/// same geometry: testdata/go.img, a sparse image of a littlefs that holds
/// /knixx.ini with GO_CONFIG, made by testdata/golfs.
#[test]
fn reads_go_littlefs() {
    let mut flash = RamFlash::from_sparse(include_bytes!("../testdata/go.img"));
    let (r, log) = load_str(&mut flash);
    assert_eq!(r.unwrap(), GO_CONFIG);
    assert_eq!(log, "mounted littlefs\n");
}

/// Makes a littlefs here that holds /knixx.ini with GO_CONFIG and reads it
/// back. With UPDATE_TESTDATA set it writes it as testdata/rust.img, a
/// sparse image, for testdata/golfs to read with the Go firmware's
/// littlefs.
#[test]
fn writes_littlefs_for_go() {
    let mut flash = RamFlash::blank();
    load_str(&mut flash).0.unwrap();
    save(&mut flash, GO_CONFIG.as_bytes()).unwrap();
    let img = flash.to_sparse();
    let path = concat!(env!("CARGO_MANIFEST_DIR"), "/testdata/rust.img");
    if std::env::var_os("UPDATE_TESTDATA").is_some() {
        std::fs::write(path, &img).unwrap();
    }
    // The image must mount and read back here too.
    let mut back = RamFlash::from_sparse(&img);
    assert_eq!(load_str(&mut back).0.unwrap(), GO_CONFIG);
}
