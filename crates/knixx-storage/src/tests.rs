use std::boxed::Box;
use std::format;
use std::string::{String, ToString};
use std::vec;
use std::vec::Vec;

use super::*;

/// A NOR flash in RAM: erased bytes are 0xFF and writes can only clear
/// bits, so a write over data that was not erased fails the test.
/// It counts the bytes read and written and the sectors erased.
struct RamFlash {
    data: Vec<u8>,
    read: u64,
    written: u64,
    erased: u64,
}

impl RamFlash {
    fn blank() -> RamFlash {
        RamFlash {
            data: vec![0xFF; SECTOR_SIZE * SECTOR_COUNT],
            read: 0,
            written: 0,
            erased: 0,
        }
    }
}

impl BlockDevice for RamFlash {
    type Error = ();

    fn read(&mut self, off: u32, buf: &mut [u8]) -> Result<(), ()> {
        let off = off as usize;
        buf.copy_from_slice(&self.data[off..off + buf.len()]);
        self.read += buf.len() as u64;
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
        self.written += data.len() as u64;
        Ok(())
    }

    fn erase_sector(&mut self, off: u32) -> Result<(), ()> {
        assert_eq!(off as usize % SECTOR_SIZE, 0, "unaligned erase at {off:#x}");
        self.data[off as usize..off as usize + SECTOR_SIZE].fill(0xFF);
        self.erased += 1;
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

#[test]
fn streamed_file() {
    let mut f = formatted();
    mkdir(&mut f, "/fw").unwrap();
    create(&mut f, "/fw/update.tmp").unwrap();
    // A firmware-sized file in 4 KiB pieces, as the fw service stages one.
    let data: Vec<u8> = (0..300_000u32).map(|i| (i ^ (i >> 8)) as u8).collect();
    let mut size = 0;
    for piece in data.chunks(4096) {
        size = append(&mut f, "/fw/update.tmp", piece).unwrap();
    }
    assert_eq!(size as usize, data.len());
    // Read back at offsets, across pieces, and past the end.
    let mut buf = [0u8; 5000];
    for off in [0usize, 1, 4095, 100_000, 299_000] {
        let n = read_at(&mut f, "/fw/update.tmp", off as u32, &mut buf).unwrap();
        let want = &data[off..(off + buf.len()).min(data.len())];
        assert_eq!(&buf[..n], want, "at {off}");
    }
    assert_eq!(
        read_at(&mut f, "/fw/update.tmp", data.len() as u32, &mut buf).unwrap(),
        0
    );
    rename(&mut f, "/fw/update.tmp", "/fw/update.kfw").unwrap();
    let got = stat(&mut f, "/fw/update.kfw", |e| e.map(|e| e.size)).unwrap();
    assert_eq!(got, Some(data.len() as u32));
    assert!(stat(&mut f, "/fw/update.tmp", |e| e.is_none()).unwrap());
    // create truncates.
    create(&mut f, "/fw/update.kfw").unwrap();
    assert_eq!(
        stat(&mut f, "/fw/update.kfw", |e| e.map(|e| e.size)).unwrap(),
        Some(0)
    );
}

#[test]
fn append_needs_the_file_and_space_shrinks() {
    let mut f = formatted();
    assert!(append(&mut f, "/missing", b"x").unwrap_err().not_found());
    let before = free(&mut f).unwrap();
    create(&mut f, "/big").unwrap();
    append(&mut f, "/big", &[0x55; 100_000]).unwrap();
    let after = free(&mut f).unwrap();
    assert!(before - after >= 100_000, "{before} -> {after}");
    remove(&mut f, "/big").unwrap();
    assert_eq!(free(&mut f).unwrap(), before);
}

/// A firmware-sized test file.
fn image(len: u32) -> Vec<u8> {
    (0..len).map(|i| (i ^ (i >> 8) ^ (i >> 16)) as u8).collect()
}

#[test]
fn session_streams_a_file() {
    let mut f = formatted();
    let mut s = Box::pin(Session::new(&mut f));
    s.as_mut().mount().unwrap();
    s.as_mut().mkdir("/fw").unwrap();
    s.as_mut().create("/fw/update.tmp").unwrap();
    // 800 KB in BLE-sized pieces, as they come.
    let data = image(800_000);
    let mut size = 0;
    for piece in data.chunks(240) {
        size = s.as_mut().append("/fw/update.tmp", piece).unwrap();
    }
    assert_eq!(size as usize, data.len());
    // read_at closes the stream and reads it whole.
    let mut back = vec![0u8; data.len()];
    let mut off = 0;
    while off < back.len() {
        let n = s
            .as_mut()
            .read_at(
                "/fw/update.tmp",
                off as u32,
                &mut back[off..(off + 4096).min(data.len())],
            )
            .unwrap();
        assert!(n > 0);
        off += n;
    }
    assert!(back == data);
    s.as_mut().rename("/fw/update.tmp", "/fw/update.kfw").unwrap();
    drop(s);
    // Another session, as after a reset, sees it all.
    assert_eq!(
        stat(&mut f, "/fw/update.kfw", |e| e.map(|e| e.size)).unwrap(),
        Some(800_000)
    );
}

#[test]
fn session_works_on_other_files_meanwhile() {
    let mut f = formatted();
    let mut s = Box::pin(Session::new(&mut f));
    s.as_mut().mount().unwrap();
    s.as_mut().create("/big").unwrap();
    let data = image(50_000);
    for (i, piece) in data.chunks(1000).enumerate() {
        s.as_mut().append("/big", piece).unwrap();
        // The config, the bonds and the scripts change during a push.
        if i % 10 == 0 {
            s.as_mut().mkdir("/bonds").unwrap();
            s.as_mut().write_atomic("/bonds/list", &[i as u8; 100]).unwrap();
            s.as_mut()
                .save(format!("[device]\nindividual = 1.1.{i}\n").as_bytes())
                .unwrap();
            let mut buf = [0u8; 200];
            assert_eq!(s.as_mut().read("/bonds/list", &mut buf).unwrap(), 100);
            assert!(s.as_mut().free().unwrap() > 1_000_000);
        }
    }
    // The stream is still the one open: stat closes it, and sees it whole.
    assert_eq!(
        s.as_mut().stat("/big", |e| e.map(|e| e.size)).unwrap(),
        Some(50_000)
    );
    let mut back = vec![0u8; 50_000];
    assert_eq!(s.as_mut().read("/big", &mut back).unwrap(), 50_000);
    assert!(back == data);
    drop(s);
    let mut buf = [0u8; config::MAX_FILE];
    let n = load(&mut f, &mut buf, &mut String::new()).unwrap();
    assert_eq!(&buf[..n], b"[device]\nindividual = 1.1.40\n");
}

#[test]
fn session_stream_ops_on_its_own_path() {
    let mut f = formatted();
    let mut s = Box::pin(Session::new(&mut f));
    s.as_mut().mount().unwrap();
    // remove of the open stream.
    s.as_mut().create("/a").unwrap();
    s.as_mut().append("/a", b"hello").unwrap();
    assert!(s.as_mut().remove("/a").unwrap());
    assert!(s.as_mut().stat("/a", |e| e.is_none()).unwrap());
    assert!(s.as_mut().append("/a", b"x").unwrap_err().not_found());
    // create over the open stream truncates it.
    s.as_mut().create("/b").unwrap();
    s.as_mut().append("/b", b"hello").unwrap();
    s.as_mut().create("/b").unwrap();
    assert_eq!(s.as_mut().append("/b", b"hi").unwrap(), 2);
    // rename onto the open stream replaces it.
    s.as_mut().create("/c").unwrap();
    s.as_mut().append("/c", b"new").unwrap();
    s.as_mut().append("/b", b"!").unwrap();
    s.as_mut().rename("/c", "/b").unwrap();
    let mut buf = [0u8; 8];
    assert_eq!(s.as_mut().read("/b", &mut buf).unwrap(), 3);
    assert_eq!(&buf[..3], b"new");
    // Switching between files commits the one before.
    s.as_mut().create("/d").unwrap();
    s.as_mut().create("/e").unwrap();
    s.as_mut().append("/d", b"dd").unwrap();
    s.as_mut().append("/e", b"eee").unwrap();
    s.as_mut().append("/d", b"d").unwrap();
    s.as_mut().close_stream().unwrap();
    assert_eq!(s.as_mut().stat("/d", |e| e.map(|e| e.size)).unwrap(), Some(3));
    assert_eq!(s.as_mut().stat("/e", |e| e.map(|e| e.size)).unwrap(), Some(3));
}

#[test]
fn session_reset_mid_stream() {
    let mut f = formatted();
    create(&mut f, "/fw.tmp").unwrap();
    append(&mut f, "/fw.tmp", &[1; 10_000]).unwrap();
    {
        // A reset: the session is never dropped, so nothing is closed or
        // unmounted.
        let mut s = Box::pin(Session::new(&mut f));
        s.as_mut().mount().unwrap();
        s.as_mut().write_atomic("/other", b"kept").unwrap();
        for _ in 0..50 {
            s.as_mut().append("/fw.tmp", &[2; 1000]).unwrap();
        }
        core::mem::forget(s);
    }
    // The file has its committed size; what was committed meanwhile stays.
    assert_eq!(
        stat(&mut f, "/fw.tmp", |e| e.map(|e| e.size)).unwrap(),
        Some(10_000)
    );
    let mut buf = [0u8; 8];
    assert_eq!(read(&mut f, "/other", &mut buf).unwrap(), 4);
    // And the filesystem takes a new stream.
    create(&mut f, "/fw.tmp").unwrap();
    append(&mut f, "/fw.tmp", &[3; 5000]).unwrap();
    assert_eq!(
        stat(&mut f, "/fw.tmp", |e| e.map(|e| e.size)).unwrap(),
        Some(5000)
    );
}

/// The flash bytes read and written, and the sectors erased, for each
/// tenth of a stream of len bytes appended in pieces, through a session or
/// with a mount per append.
fn stream_cost(len: u32, piece: usize, session: bool) -> Vec<(u64, u64, u64)> {
    let mut f = formatted();
    create(&mut f, "/fw.tmp").unwrap();
    let data = image(len);
    let tenth = data.len().div_ceil(10);
    let mut out = Vec::new();
    let counts = |f: &RamFlash| (f.read, f.written, f.erased);
    if session {
        let mut s = Box::pin(Session::new(&mut f));
        s.as_mut().mount().unwrap();
        for part in data.chunks(tenth) {
            let before = counts(s.dev);
            for p in part.chunks(piece) {
                s.as_mut().append("/fw.tmp", p).unwrap();
            }
            let after = counts(s.dev);
            out.push((after.0 - before.0, after.1 - before.1, after.2 - before.2));
        }
        s.as_mut().close_stream().unwrap();
    } else {
        for part in data.chunks(tenth) {
            let before = counts(&f);
            for p in part.chunks(piece) {
                append(&mut f, "/fw.tmp", p).unwrap();
            }
            let after = counts(&f);
            out.push((after.0 - before.0, after.1 - before.1, after.2 - before.2));
        }
    }
    assert_eq!(stat(&mut f, "/fw.tmp", |e| e.map(|e| e.size)).unwrap(), Some(len));
    out
}

#[test]
fn session_append_cost_stays_flat() {
    // A firmware of 800 KB in 4 KiB pieces.
    let len = 800_000;
    let per_mount = stream_cost(len, 4096, false);
    let session = stream_cost(len, 4096, true);
    let small = stream_cost(len, 240, true);
    let total = |v: &[(u64, u64, u64)]| v.iter().fold((0, 0, 0), |a, b| (a.0 + b.0, a.1 + b.1, a.2 + b.2));
    std::println!("bytes read, written, sectors erased for {len} bytes:");
    std::println!("  mount per 4 KiB append: {:?}", total(&per_mount));
    std::println!("  session, 4 KiB appends: {:?}", total(&session));
    std::println!("  session, 240 B appends: {:?}", total(&small));
    std::println!(
        "  reads per tenth, mount per append: {:?}",
        per_mount.iter().map(|c| c.0).collect::<Vec<_>>()
    );
    std::println!(
        "  reads per tenth, session:          {:?}",
        session.iter().map(|c| c.0).collect::<Vec<_>>()
    );
    // A mount per append reads more and more as the file grows; a session
    // reads about the same for each tenth.
    assert!(per_mount[9].0 > per_mount[0].0 * 3 / 2, "{per_mount:?}");
    let max = session.iter().map(|c| c.0).max().unwrap();
    let min = session.iter().map(|c| c.0).min().unwrap();
    assert!(max <= 2 * min.max(4096), "{session:?}");
    // And far less overall, in reads and in writes.
    assert!(total(&session).0 * 10 < total(&per_mount).0);
    assert!(total(&session).1 < total(&per_mount).1);
    // Small pieces cost no more flash work than large ones.
    assert!(total(&small).1 <= total(&session).1 + 4096);
}
