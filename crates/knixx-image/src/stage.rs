//! Staging an image while the firmware runs: the .kfw file arrives piece by
//! piece (over the BLE transfer service, knixx-xfer) and goes to
//! STAGED_TMP, which is renamed to STAGED_PATH once the whole file checks
//! out. The bootloader installs it from there.
//!
//! The header is checked as soon as it is in: its size against MAX_IMAGE
//! and the free space, its key, and its counter against the running
//! firmware's. The signature and the CRC are checked as the body streams
//! through, and at the end the file is read back, so that what lies on the
//! flash is what was signed.

use crate::{
    Crc32, HEADER_SIZE, Header, Keys, Refusal, STAGED_PATH, STAGED_TMP, Verifier, check_counter, check_size,
};

/// The bytes written to the file at a time, and read back at a time. The
/// firmware keeps the file open between appends (knixx_storage::Session),
/// so a piece need not be large: a sector is enough.
pub const PIECE: usize = 4 * 1024;

/// The space the littlefs keeps beyond the image, for the configuration,
/// the scripts and the bonds to change meanwhile.
pub const SPARE: u32 = 64 * 1024;

/// The files a staged image goes to: the littlefs, or a fake in tests.
pub trait Files {
    type Error;
    /// Makes path empty.
    fn create(&mut self, path: &str) -> Result<(), Self::Error>;
    /// Appends data to path and returns its new size.
    fn append(&mut self, path: &str, data: &[u8]) -> Result<u32, Self::Error>;
    /// Reads path from off into buf, returning the bytes read.
    fn read_at(&mut self, path: &str, off: u32, buf: &mut [u8]) -> Result<usize, Self::Error>;
    /// Renames from to to, replacing to.
    fn rename(&mut self, from: &str, to: &str) -> Result<(), Self::Error>;
    /// Removes path, if it is there.
    fn remove(&mut self, path: &str) -> Result<(), Self::Error>;
    /// The free space, in bytes.
    fn free(&mut self) -> Result<u32, Self::Error>;
}

/// Why staging failed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StageError<E> {
    /// The image is refused: its size, header, key, counter, CRC or
    /// signature.
    Refused(Refusal),
    /// The littlefs has not enough room.
    Space {
        need: u32,
        free: u32,
    },
    /// More bytes than the size given.
    Overflow,
    /// The size given does not match the header's.
    SizeMismatch {
        given: u32,
        header: u32,
    },
    /// The file's CRC does not match what the sender gave.
    TransportCrc,
    /// The file read back differs from what was received.
    ReadBack,
    /// finish before all the bytes.
    Incomplete {
        got: u32,
        size: u32,
    },
    Files(E),
}

impl<E: core::fmt::Display> core::fmt::Display for StageError<E> {
    fn fmt(&self, f: &mut core::fmt::Formatter) -> core::fmt::Result {
        match self {
            StageError::Refused(r) => r.fmt(f),
            StageError::Space { need, free } => {
                write!(f, "no room on the flash: {need} bytes needed, {free} free")
            }
            StageError::Overflow => f.write_str("more bytes than the size given"),
            StageError::SizeMismatch { given, header } => {
                write!(f, "{given} bytes given, the header says {header}")
            }
            StageError::TransportCrc => f.write_str("CRC mismatch: the file was damaged on the way"),
            StageError::ReadBack => f.write_str("the file read back from the flash differs"),
            StageError::Incomplete { got, size } => write!(f, "{got} of {size} bytes"),
            StageError::Files(e) => e.fmt(f),
        }
    }
}

impl<E> From<Refusal> for StageError<E> {
    fn from(r: Refusal) -> Self {
        StageError::Refused(r)
    }
}

/// An image being staged.
pub struct Stager {
    size: u32,
    got: u32,
    raw: [u8; HEADER_SIZE],
    verifier: Option<Verifier>,
    crc: Crc32,
    buf: [u8; PIECE],
    buf_len: usize,
}

impl Stager {
    /// Starts staging a .kfw file of size bytes: checks the size and the
    /// room for it, and makes STAGED_TMP empty.
    pub fn begin<F: Files>(files: &mut F, size: u32) -> Result<Stager, StageError<F::Error>> {
        check_size(size)?;
        files.remove(STAGED_TMP).map_err(StageError::Files)?;
        let free = files.free().map_err(StageError::Files)?;
        let need = size.saturating_add(SPARE);
        if free < need {
            return Err(StageError::Space { need, free });
        }
        files.create(STAGED_TMP).map_err(StageError::Files)?;
        Ok(Stager {
            size,
            got: 0,
            raw: [0; HEADER_SIZE],
            verifier: None,
            crc: Crc32::new(),
            buf: [0; PIECE],
            buf_len: 0,
        })
    }

    /// The bytes received.
    pub fn received(&self) -> u32 {
        self.got
    }

    /// The size of the file being staged.
    pub fn size(&self) -> u32 {
        self.size
    }

    /// The header, once it is in and checked.
    pub fn header(&self) -> Option<&Header> {
        self.verifier.as_ref().map(Verifier::header)
    }

    /// Takes the next bytes of the file. The header is checked as soon as
    /// it is complete: signed with one of keys, its counter above
    /// running's.
    pub fn feed<F: Files>(
        &mut self,
        files: &mut F,
        mut data: &[u8],
        keys: Keys,
        running: Option<&Header>,
    ) -> Result<(), StageError<F::Error>> {
        if data.len() as u32 > self.size - self.got {
            return Err(StageError::Overflow);
        }
        self.crc.update(data);
        while !data.is_empty() {
            let got = self.got as usize;
            let n = if got < HEADER_SIZE {
                let n = (HEADER_SIZE - got).min(data.len());
                self.raw[got..got + n].copy_from_slice(&data[..n]);
                if got + n == HEADER_SIZE {
                    let v = Verifier::new(&self.raw, keys)?;
                    let header = v.header().file_size();
                    if header != self.size {
                        return Err(StageError::SizeMismatch {
                            given: self.size,
                            header,
                        });
                    }
                    check_counter(v.header(), running, false)?;
                    self.verifier = Some(v);
                }
                n
            } else {
                // Past the header, which was checked when it completed.
                if let Some(v) = self.verifier.as_mut() {
                    v.update(data);
                }
                data.len()
            };
            self.push(files, &data[..n])?;
            self.got += n as u32;
            data = &data[n..];
        }
        Ok(())
    }

    /// Buffers data, appending to the file whenever a piece is full.
    fn push<F: Files>(&mut self, files: &mut F, mut data: &[u8]) -> Result<(), StageError<F::Error>> {
        while !data.is_empty() {
            let n = (PIECE - self.buf_len).min(data.len());
            self.buf[self.buf_len..self.buf_len + n].copy_from_slice(&data[..n]);
            self.buf_len += n;
            data = &data[n..];
            if self.buf_len == PIECE {
                self.flush(files)?;
            }
        }
        Ok(())
    }

    fn flush<F: Files>(&mut self, files: &mut F) -> Result<(), StageError<F::Error>> {
        if self.buf_len > 0 {
            files
                .append(STAGED_TMP, &self.buf[..self.buf_len])
                .map_err(StageError::Files)?;
            self.buf_len = 0;
        }
        Ok(())
    }

    /// Ends staging: writes what is buffered, checks the signature and the
    /// CRC (and crc, the sender's CRC-32 of the whole file, if given), reads
    /// the file back, and renames it to STAGED_PATH.
    pub fn finish<F: Files>(
        mut self,
        files: &mut F,
        crc: Option<u32>,
    ) -> Result<Header, StageError<F::Error>> {
        if self.got != self.size {
            return Err(StageError::Incomplete {
                got: self.got,
                size: self.size,
            });
        }
        self.flush(files)?;
        if crc.is_some_and(|c| c != self.crc.finish()) {
            return Err(StageError::TransportCrc);
        }
        let v = self.verifier.take().ok_or(StageError::Incomplete {
            got: self.got,
            size: self.size,
        })?;
        let hdr = v.finish()?;
        // Read back: the whole file's CRC, against the one received.
        let mut back = Crc32::new();
        let mut off = 0u32;
        while off < self.size {
            let n = files
                .read_at(STAGED_TMP, off, &mut self.buf)
                .map_err(StageError::Files)?;
            if n == 0 {
                return Err(StageError::ReadBack);
            }
            back.update(&self.buf[..n]);
            off += n as u32;
        }
        if off != self.size || back.finish() != self.crc.finish() {
            return Err(StageError::ReadBack);
        }
        files.rename(STAGED_TMP, STAGED_PATH).map_err(StageError::Files)?;
        Ok(hdr)
    }
}

/// Reads the header of the staged image, if there is one and it parses.
pub fn staged<F: Files>(files: &mut F) -> Option<Header> {
    let mut raw = [0u8; HEADER_SIZE];
    match files.read_at(STAGED_PATH, 0, &mut raw) {
        Ok(HEADER_SIZE) => Header::parse(&raw).ok(),
        _ => None,
    }
}
