//! bolt genpkg: building the DFU package of an application hex file, as
//! adafruit-nrfutil dfu genpkg does.

use std::fs;
use std::io::Cursor;
use std::path::Path;

use anyhow::{Context, Result};
use knixx_host::legacydfu::{InitPacket, Package, crc16, read_hex};

/// Builds the DFU package out from the application hex file input. The zip
/// is written through a temporary file, so that a failed run leaves no
/// partial package.
pub fn run(input: &Path, out: &Path, ip: &InitPacket) -> Result<()> {
    let text = fs::read_to_string(input).with_context(|| input.display().to_string())?;
    let (base, bin) = read_hex(&text).with_context(|| input.display().to_string())?;
    let name = input
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default();
    let crc = crc16(&bin);
    let n = bin.len();
    let mut zip = Cursor::new(Vec::new());
    Package::new(bin, ip).write_zip(&mut zip, &name, ip)?;
    crate::write_atomic(out, &zip.into_inner())?;
    eprintln!(
        "{}: application of {n} bytes at {base:#x}, crc16 {crc:#06x}",
        out.display()
    );
    Ok(())
}
