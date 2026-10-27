//! Reading an Intel HEX file into an application image, as adafruit-nrfutil
//! dfu genpkg does.

use std::fmt;

/// The address limits of an application image, as adafruit-nrfutil
/// converts a hex file: the MBR below MBR_END and the UICR from UICR_START
/// on are left out.
const MBR_END: u32 = 0x1000;
const UICR_START: u32 = 0x1000_0000;

/// An error in a hex file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HexError(String);

impl fmt::Display for HexError {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for HexError {}

fn err<T>(s: impl Into<String>) -> Result<T, HexError> {
    Err(HexError(s.into()))
}

/// Reads an Intel HEX file into one contiguous image: from the lowest
/// address, at least MBR_END, to the highest below UICR_START, with the gaps
/// filled with 0xFF and the size rounded up to a multiple of 4. It returns
/// the image's start address and the image.
pub fn read_hex(text: &str) -> Result<(u32, Vec<u8>), HexError> {
    let mut chunks: Vec<(u32, Vec<u8>)> = Vec::new();
    let mut upper = 0u32; // the extended linear or segment address
    let mut eof = false;
    for (i, line) in text.lines().enumerate() {
        let n = i + 1;
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        if eof {
            return err(format!("hex line {n}: data after the end of file record"));
        }
        let rec = parse_record(line).map_err(|e| HexError(format!("hex line {n}: {e}")))?;
        let data = &rec[4..];
        match rec[3] {
            0x00 => {
                let addr = upper.wrapping_add(u32::from(rec[1]) << 8 | u32::from(rec[2]));
                chunks.push((addr, data.to_vec()));
            }
            0x01 => eof = true,
            typ @ (0x02 | 0x04) => {
                if data.len() != 2 {
                    return err(format!("hex line {n}: address record of {} bytes", data.len()));
                }
                upper = u32::from(data[0]) << 8 | u32::from(data[1]);
                upper <<= if typ == 0x02 { 4 } else { 16 };
            }
            // A start address, which the image does not hold.
            0x03 | 0x05 => {}
            typ => return err(format!("hex line {n}: record type {typ:#04x}")),
        }
    }
    if !eof {
        return err("hex: no end of file record");
    }

    // The extent of the data in the application area.
    let (mut lo, mut hi) = (UICR_START, 0u32);
    for (addr, data) in &chunks {
        let start = (*addr).max(MBR_END);
        let end = (u64::from(*addr) + data.len() as u64).min(u64::from(UICR_START)) as u32;
        if start < end {
            lo = lo.min(start);
            hi = hi.max(end);
        }
    }
    if lo >= hi {
        return err("hex: no application data");
    }
    let len = ((hi - lo + 3) & !3) as usize;
    let mut image = vec![0xFFu8; len];
    let mut written = vec![false; len];
    for (addr, data) in &chunks {
        for (i, &b) in data.iter().enumerate() {
            let a = u64::from(*addr) + i as u64;
            if a < u64::from(lo) || a >= u64::from(hi) {
                continue;
            }
            let off = (a - u64::from(lo)) as usize;
            if written[off] {
                return err(format!("hex: address {a:#x} written twice"));
            }
            written[off] = true;
            image[off] = b;
        }
    }
    Ok((lo, image))
}

/// Decodes one record, ":llaaaatt<data>cc", and checks its length and
/// checksum. It returns the bytes from the length to the data.
fn parse_record(line: &str) -> Result<Vec<u8>, String> {
    let Some(hex) = line.strip_prefix(':') else {
        return Err("no record start".into());
    };
    if hex.len() % 2 != 0 || !hex.is_ascii() {
        return Err("bad hex digits".into());
    }
    let rec = (0..hex.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&hex[i..i + 2], 16))
        .collect::<Result<Vec<u8>, _>>()
        .map_err(|e| e.to_string())?;
    if rec.len() < 5 || rec.len() != 5 + usize::from(rec[0]) {
        return Err("bad record length".into());
    }
    if rec.iter().fold(0u8, |s, &b| s.wrapping_add(b)) != 0 {
        return Err("bad checksum".into());
    }
    Ok(rec[..rec.len() - 1].to_vec())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Returns the hex record of type typ at addr with data.
    pub(crate) fn record(typ: u8, addr: u16, data: &[u8]) -> String {
        let mut rec = vec![data.len() as u8, (addr >> 8) as u8, addr as u8, typ];
        rec.extend_from_slice(data);
        let sum = rec.iter().fold(0u8, |s, &b| s.wrapping_add(b));
        let mut s = String::from(":");
        for b in rec {
            s.push_str(&format!("{b:02X}"));
        }
        s.push_str(&format!("{:02X}\n", sum.wrapping_neg()));
        s
    }

    #[test]
    fn read() {
        let hex = record(0x04, 0, &[0x00, 0x02]) // 0x20000
            + &record(0x00, 0x6000, &[1, 2, 3, 4])
            + &record(0x00, 0x6008, &[5]) // a gap of 4, an odd end
            + &record(0x05, 0, &[0, 2, 0x60, 0])
            + &record(0x04, 0, &[0x10, 0x00]) // the UICR, left out
            + &record(0x00, 0x1014, &[0xaa, 0xbb, 0xcc, 0xdd])
            + &record(0x01, 0, &[]);
        let want = vec![1, 2, 3, 4, 0xff, 0xff, 0xff, 0xff, 5, 0xff, 0xff, 0xff];
        assert_eq!(read_hex(&hex), Ok((0x26000, want)));
    }

    #[test]
    fn mbr_left_out() {
        // Data below 0x1000 is the MBR's and left out.
        let hex = record(0x00, 0x0ffe, &[1, 2, 3, 4]) + &record(0x01, 0, &[]);
        assert_eq!(read_hex(&hex), Ok((0x1000, vec![3, 4, 0xff, 0xff])));
    }

    #[test]
    fn errors() {
        let data = record(0x00, 0x6000, &[1, 2, 3, 4]);
        let eof = record(0x01, 0, &[]);
        for (name, hex) in [
            ("no eof", data.clone()),
            ("bad checksum", data.replace("01020304", "01020305") + &eof),
            ("bad length", ":0560000001020304F2\n".to_string() + &eof),
            ("no colon", data[1..].to_string() + &eof),
            ("bad type", record(0x06, 0, &[]) + &eof),
            ("overlap", data.clone() + &record(0x00, 0x6002, &[9]) + &eof),
            ("after eof", eof.clone() + &data),
            ("empty", eof.clone()),
        ] {
            assert!(read_hex(&hex).is_err(), "{name}: no error");
        }
    }
}
