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

/// The names, sizes and kinds of the entries of dir, sorted.
fn ls(flash: &mut RamFlash, dir: &str) -> Vec<(String, u32, bool)> {
    let mut v = Vec::new();
    list(flash, dir, |e| v.push((e.name.to_string(), e.size, e.dir))).unwrap();
    v.sort();
    v
}

/// A flash with the default configuration on it.
fn formatted() -> RamFlash {
    let mut flash = RamFlash::blank();
    load_str(&mut flash).0.unwrap();
    flash
}

#[test]
fn files_in_a_directory() {
    let mut flash = formatted();
    mkdir(&mut flash, "/scripts").unwrap();
    mkdir(&mut flash, "/scripts").unwrap(); // there already: fine
    let big: Vec<u8> = (0..10_000u32).map(|i| i as u8).collect();
    write_atomic(&mut flash, "/scripts/a.rhgn", b"first").unwrap();
    write_atomic(&mut flash, "/scripts/b.rhgn", &big).unwrap();
    // A shorter file replaces the old one whole.
    write_atomic(&mut flash, "/scripts/a.rhgn", b"2nd").unwrap();

    let mut buf = [0u8; 16];
    assert_eq!(read(&mut flash, "/scripts/a.rhgn", &mut buf), Ok(3));
    assert_eq!(&buf[..3], b"2nd");
    let mut buf = vec![0u8; 10_000];
    assert_eq!(read(&mut flash, "/scripts/b.rhgn", &mut buf), Ok(10_000));
    assert!(buf == big, "b.rhgn read back differs");

    // No .tmp left behind; the directory shows in the root.
    assert_eq!(
        ls(&mut flash, "/scripts"),
        [
            ("a.rhgn".to_string(), 3, false),
            ("b.rhgn".to_string(), 10_000, false)
        ]
    );
    assert_eq!(
        ls(&mut flash, "/"),
        [
            ("knixx.ini".to_string(), config::DEFAULT.len() as u32, false),
            ("scripts".to_string(), 0, true)
        ]
    );
    assert_eq!(
        stat(&mut flash, "/scripts/b.rhgn", |e| e.map(|e| (e.size, e.dir))),
        Ok(Some((10_000, false)))
    );
    assert_eq!(stat(&mut flash, "/scripts/c.rhgn", |e| e.is_some()), Ok(false));

    assert_eq!(remove(&mut flash, "/scripts/a.rhgn"), Ok(true));
    assert_eq!(remove(&mut flash, "/scripts/a.rhgn"), Ok(false));
    assert_eq!(ls(&mut flash, "/scripts").len(), 1);

    // The configuration is untouched, and it survives a remount.
    assert_eq!(load_str(&mut flash).0.unwrap().as_bytes(), config::DEFAULT);
}

#[test]
fn file_errors() {
    let mut flash = formatted();
    let mut buf = [0u8; 4];
    let e = read(&mut flash, "/nope", &mut buf).unwrap_err();
    assert!(e.not_found(), "{e:?}");
    assert_eq!(e.to_string(), "no such file or directory");
    // Larger than the buffer.
    write_atomic(&mut flash, "/five", b"12345").unwrap();
    assert_eq!(read(&mut flash, "/five", &mut buf), Err(FsError::TooLarge));
    // A directory is not a file.
    mkdir(&mut flash, "/d").unwrap();
    assert!(matches!(read(&mut flash, "/d", &mut buf), Err(FsError::Fs(_))));
    // Bad paths, refused before littlefs sees them.
    let long = "/".repeat(MAX_PATH + 1);
    for p in ["", "/a\0b", long.as_str()] {
        assert_eq!(write_atomic(&mut flash, p, b"x"), Err(FsError::Path), "{p:?}");
    }
    // A file in a missing directory.
    assert!(write_atomic(&mut flash, "/none/x", b"x").unwrap_err().not_found());
    // A blank flash is not mounted, and not formatted either.
    let mut blank = RamFlash::blank();
    assert!(matches!(mkdir(&mut blank, "/scripts"), Err(FsError::Mount(_))));
    assert!(!has_superblock(&mut blank));
}
