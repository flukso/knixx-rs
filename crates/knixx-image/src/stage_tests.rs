use std::collections::BTreeMap;
use std::string::{String, ToString};
use std::vec::Vec;

use crate::stage::{Files, SPARE, StageError, Stager, staged};
use crate::*;

/// Files in RAM, with a free space and an optional fault.
struct Ram {
    files: BTreeMap<String, Vec<u8>>,
    free: u32,
    /// Flip a bit of what read_at returns, to fake a bad flash.
    corrupt_reads: bool,
}

impl Ram {
    fn new() -> Ram {
        Ram {
            files: BTreeMap::new(),
            free: 2 * 1024 * 1024,
            corrupt_reads: false,
        }
    }
}

impl Files for Ram {
    type Error = &'static str;
    fn create(&mut self, p: &str) -> Result<(), &'static str> {
        self.files.insert(p.to_string(), Vec::new());
        Ok(())
    }
    fn append(&mut self, p: &str, d: &[u8]) -> Result<u32, &'static str> {
        let f = self.files.get_mut(p).ok_or("no file")?;
        f.extend_from_slice(d);
        Ok(f.len() as u32)
    }
    fn read_at(&mut self, p: &str, off: u32, buf: &mut [u8]) -> Result<usize, &'static str> {
        let f = self.files.get(p).ok_or("no file")?;
        let off = (off as usize).min(f.len());
        let n = buf.len().min(f.len() - off);
        buf[..n].copy_from_slice(&f[off..off + n]);
        if self.corrupt_reads && n > 0 {
            buf[0] ^= 1;
        }
        Ok(n)
    }
    fn rename(&mut self, from: &str, to: &str) -> Result<(), &'static str> {
        let f = self.files.remove(from).ok_or("no file")?;
        self.files.insert(to.to_string(), f);
        Ok(())
    }
    fn remove(&mut self, p: &str) -> Result<(), &'static str> {
        self.files.remove(p);
        Ok(())
    }
    fn free(&mut self) -> Result<u32, &'static str> {
        Ok(self.free)
    }
}

const SEED: [u8; 32] = [3; 32];

fn keys() -> [(u8, [u8; 32]); 1] {
    [(1, sign::public_key(&SEED))]
}

fn kfw(len: usize, counter: u32) -> Vec<u8> {
    let body: Vec<u8> = (0..len).map(|i| (i * 13) as u8).collect();
    sign::kfw(Header::new(&body, counter, "test", 0), &body, &SEED, 1)
}

fn stage(
    fs: &mut Ram,
    file: &[u8],
    piece: usize,
    running: Option<&Header>,
) -> Result<Header, StageError<&'static str>> {
    let k = keys();
    let mut s = Stager::begin(fs, file.len() as u32)?;
    for p in file.chunks(piece) {
        s.feed(fs, p, Keys(&k), running)?;
    }
    s.finish(fs, Some(crc32(file)))
}

#[test]
fn stages_in_any_pieces() {
    let file = kfw(20_000, 9);
    for piece in [1, 20, 244, 2048, 4096, 50_000] {
        let mut fs = Ram::new();
        let h = stage(&mut fs, &file, piece, None).unwrap();
        assert_eq!(h.counter, 9);
        assert_eq!(fs.files[STAGED_PATH], file, "piece {piece}");
        assert!(!fs.files.contains_key(STAGED_TMP));
        assert_eq!(staged(&mut fs), Some(h));
    }
}

#[test]
fn refusals_before_any_body() {
    let file = kfw(5000, 9);
    let k = keys();
    // A downgrade is refused once the header is in.
    let running = Header::new(&[1], 10, "run", 0);
    let mut fs = Ram::new();
    let mut s = Stager::begin(&mut fs, file.len() as u32).unwrap();
    let r = s.feed(&mut fs, &file[..HEADER_SIZE], Keys(&k), Some(&running));
    assert_eq!(
        r,
        Err(StageError::Refused(Refusal::Downgrade {
            running: 10,
            image: 9
        }))
    );
    // A wrong key.
    let mut fs = Ram::new();
    let mut s = Stager::begin(&mut fs, file.len() as u32).unwrap();
    let other = [(1, sign::public_key(&[4; 32]))];
    let mut v = s.feed(&mut fs, &file[..HEADER_SIZE], Keys(&other), None);
    assert_eq!(v, Ok(()), "the signature is only known at the end");
    v = s.feed(&mut fs, &file[HEADER_SIZE..], Keys(&other), None);
    assert_eq!(v, Ok(()));
    assert_eq!(
        s.finish(&mut fs, None).unwrap_err(),
        StageError::Refused(Refusal::Signature)
    );
    assert!(!fs.files.contains_key(STAGED_PATH));
    // A size that does not match the header.
    let mut fs = Ram::new();
    let mut s = Stager::begin(&mut fs, file.len() as u32 + 4).unwrap();
    let r = s.feed(&mut fs, &file[..HEADER_SIZE], Keys(&k), None);
    assert!(matches!(r, Err(StageError::SizeMismatch { .. })));
}

#[test]
fn caps_and_space() {
    let mut fs = Ram::new();
    assert!(matches!(
        Stager::begin(&mut fs, MAX_IMAGE + 1),
        Err(StageError::Refused(Refusal::TooLarge(_)))
    ));
    fs.free = 100_000;
    assert_eq!(
        Stager::begin(&mut fs, 50_000).err(),
        Some(StageError::Space {
            need: 50_000 + SPARE,
            free: 100_000
        })
    );
    // More bytes than given.
    let file = kfw(1000, 1);
    let mut fs = Ram::new();
    let k = keys();
    let mut s = Stager::begin(&mut fs, file.len() as u32).unwrap();
    s.feed(&mut fs, &file, Keys(&k), None).unwrap();
    assert_eq!(s.feed(&mut fs, &[0], Keys(&k), None), Err(StageError::Overflow));
}

#[test]
fn damage_is_caught() {
    let file = kfw(9000, 2);
    // The sender's CRC does not match.
    let mut fs = Ram::new();
    let k = keys();
    let mut s = Stager::begin(&mut fs, file.len() as u32).unwrap();
    s.feed(&mut fs, &file, Keys(&k), None).unwrap();
    assert_eq!(s.finish(&mut fs, Some(0)).unwrap_err(), StageError::TransportCrc);
    // The flash reads back wrong.
    let mut fs = Ram::new();
    fs.corrupt_reads = true;
    let mut s = Stager::begin(&mut fs, file.len() as u32).unwrap();
    s.feed(&mut fs, &file, Keys(&k), None).unwrap();
    assert_eq!(s.finish(&mut fs, None).unwrap_err(), StageError::ReadBack);
    assert!(!fs.files.contains_key(STAGED_PATH));
    // Too early.
    let mut fs = Ram::new();
    let mut s = Stager::begin(&mut fs, file.len() as u32).unwrap();
    s.feed(&mut fs, &file[..100], Keys(&k), None).unwrap();
    assert!(matches!(
        s.finish(&mut fs, None),
        Err(StageError::Incomplete { got: 100, .. })
    ));
}

mod frames {
    use crate::stage::{FRAME_HEADER, Frame, Frames, frame};
    use std::vec::Vec;

    fn framed(offset: u32, data: &[u8]) -> Vec<u8> {
        let mut out = std::vec![0u8; FRAME_HEADER + data.len()];
        let n = frame(offset, data, &mut out);
        out.truncate(n);
        out
    }

    #[test]
    fn chunks_in_order() {
        let file: Vec<u8> = (0..10_000u32).map(|i| i as u8).collect();
        let mut f = Frames::new(file.len() as u32, 4096);
        let mut got = Vec::new();
        for (i, c) in file.chunks(4096).enumerate() {
            let fr = framed(i as u32 * 4096, c);
            // In USB packets of 64 bytes.
            let mut last = Frame::Partial;
            for p in fr.chunks(64) {
                last = f.feed(p);
                if let Frame::Chunk(_, d) = &last {
                    got.extend_from_slice(d);
                }
            }
            assert!(matches!(last, Frame::Chunk(o, _) if o == i as u32 * 4096));
        }
        assert_eq!(got, file);
        assert_eq!(f.next(), 10_000);
    }

    #[test]
    fn damage_duplicates_and_losses() {
        let file: Vec<u8> = (0..9000u32).map(|i| (i * 7) as u8).collect();
        let mut f = Frames::new(file.len() as u32, 4096);
        // A damaged frame: resend from 0.
        let mut fr = framed(0, &file[..4096]);
        fr[100] ^= 1;
        assert_eq!(f.feed(&fr), Frame::Resend(0));
        // The good one.
        assert!(matches!(f.feed(&framed(0, &file[..4096])), Frame::Chunk(0, _)));
        // The same chunk again (a stale resend): resend from 4096, not taken.
        assert_eq!(f.feed(&framed(0, &file[..4096])), Frame::Resend(4096));
        // A frame with a packet lost: partial; dropped after a stall; then
        // the whole frame again.
        let fr = framed(4096, &file[4096..8192]);
        let mut short = fr.clone();
        short.drain(640..704);
        assert_eq!(f.feed(&short), Frame::Partial);
        assert!(f.partial());
        f.drop_partial();
        assert!(matches!(f.feed(&fr), Frame::Chunk(4096, _)));
        // The short last chunk.
        assert!(matches!(f.feed(&framed(8192, &file[8192..])), Frame::Chunk(8192, d) if d.len() == 808));
        assert_eq!(f.next(), 9000);
    }
}
