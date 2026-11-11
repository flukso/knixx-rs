//! Installs the staged image, /fw/update.kfw on the QSPI flash's littlefs:
//! it checks the whole file first (size, header, CRC, signature, counter),
//! and only then erases internal flash and copies it, the header last.
//!
//! A reset during the copy leaves no valid application, so the next boot
//! installs again: the copy can be repeated as often as it takes. The
//! bootloader only reads the littlefs; the application removes the file
//! once it runs it.

use embassy_nrf::nvmc::Nvmc;
use knixx_image::{self as image, HEADER_SIZE, Header, InstallStep, Refusal, Verifier, result};
use knixx_qspi::QspiFlash;

use crate::writer::Writer;
use crate::{Irqs, app, keys, led, reset};

/// The piece read from the littlefs at a time.
const PIECE: usize = 4096;

/// The staged image, checked: its header raw and parsed.
struct Staged {
    raw: [u8; HEADER_SIZE],
    hdr: Header,
}

/// Why the staged file could not be used.
enum Bad {
    /// It could not be read: no littlefs, or a read error.
    Read,
    Refused(Refusal),
}

pub fn run(running: Option<Header>) -> ! {
    let p = embassy_nrf::init(Default::default());
    led::start(led::State::Writing);
    let mut qspi = QspiFlash::new(p.QSPI, Irqs, crate::qspi_pins!(p));
    let staged = check(&mut qspi);
    let step = image::install_step(
        match &staged {
            None => None,
            Some(Ok(s)) => Some(Ok(s.hdr)),
            Some(Err(Bad::Refused(r))) => Some(Err(*r)),
            Some(Err(Bad::Read)) => Some(Err(Refusal::Length)),
        },
        running.as_ref(),
    );
    let refused = match &staged {
        Some(Err(Bad::Refused(r))) => result::REFUSED | r.code(),
        Some(Err(Bad::Read)) => result::READ,
        _ => result::NONE,
    };
    match step {
        InstallStep::Boot => reset(0, refused),
        InstallStep::Recovery => reset(image::GPREGRET_NO_APP, refused),
        InstallStep::Copy => {}
    }
    let Some(Ok(s)) = staged else {
        reset(image::GPREGRET_NO_APP, refused)
    };
    if let Err(r) = image::check_counter(&s.hdr, running.as_ref(), false) {
        reset(0, result::REFUSED | r.code());
    }
    let mut nvmc = Nvmc::new(p.NVMC);
    match copy(&mut qspi, &mut nvmc, &s) {
        Ok(()) => reset(0, result::INSTALLED),
        // No valid application now: the next boot installs again.
        Err(()) => reset(0, result::WRITE),
    }
}

/// Checks the staged file: None without one.
fn check(qspi: &mut QspiFlash) -> Option<Result<Staged, Bad>> {
    let size = match knixx_storage::stat(qspi, image::STAGED_PATH, |e| e.map(|e| e.size)) {
        Ok(Some(n)) => n,
        Ok(None) => return None,
        Err(e) if e.not_found() => return None,
        Err(_) => return Some(Err(Bad::Read)),
    };
    Some(verify(qspi, size))
}

fn verify(qspi: &mut QspiFlash, size: u32) -> Result<Staged, Bad> {
    image::check_size(size).map_err(Bad::Refused)?;
    let mut raw = [0u8; HEADER_SIZE];
    match knixx_storage::read_at(qspi, image::STAGED_PATH, 0, &mut raw) {
        Ok(HEADER_SIZE) => {}
        _ => return Err(Bad::Read),
    }
    let mut v = Verifier::new(&raw, keys()).map_err(Bad::Refused)?;
    if v.header().file_size() != size {
        return Err(Bad::Refused(Refusal::Length));
    }
    let mut buf = [0u8; PIECE];
    let mut off = HEADER_SIZE as u32;
    while off < size {
        let n = knixx_storage::read_at(qspi, image::STAGED_PATH, off, &mut buf).map_err(|_| Bad::Read)?;
        if n == 0 {
            return Err(Bad::Refused(Refusal::Length));
        }
        v.update(&buf[..n]);
        off += n as u32;
    }
    let hdr = v.finish().map_err(Bad::Refused)?;
    Ok(Staged { raw, hdr })
}

/// Copies the checked image into internal flash and checks the CRC of what
/// it wrote before it writes the header.
fn copy(qspi: &mut QspiFlash, nvmc: &mut Nvmc<'static>, s: &Staged) -> Result<(), ()> {
    let mut w = Writer::begin(nvmc).map_err(|_| ())?;
    let mut buf = [0u8; PIECE];
    let mut off = 0u32;
    while off < s.hdr.len {
        let want = PIECE.min((s.hdr.len - off) as usize);
        let n = knixx_storage::read_at(
            qspi,
            image::STAGED_PATH,
            HEADER_SIZE as u32 + off,
            &mut buf[..want],
        )
        .map_err(|_| ())?;
        if n != want {
            return Err(());
        }
        w.write(nvmc, &buf[..n]).map_err(|_| ())?;
        off += n as u32;
    }
    w.flush(nvmc).map_err(|_| ())?;
    if image::crc32(app::body(s.hdr.len)) != s.hdr.body_crc {
        return Err(());
    }
    w.commit(nvmc, &s.raw).map_err(|_| ())
}
