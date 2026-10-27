//! Building a DFU package as adafruit-nrfutil dfu genpkg does: the image,
//! its legacy init packet and the manifest.json, in a zip.

use std::io::{Seek, Write};

use zip::CompressionMethod;
use zip::write::{SimpleFileOptions, ZipWriter};

use super::Package;

/// The fields of the legacy init packet, which the bootloader checks before
/// it takes an image.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InitPacket {
    /// The device type, 0x0052 for the Adafruit nRF52 boards.
    pub dev_type: u16,
    /// The device revision, 0xFFFF for any.
    pub dev_rev: u16,
    /// The application version, 0xFFFFFFFF for any.
    pub app_ver: u32,
    /// The SoftDevice firmware ids the image runs on.
    pub sd_req: Vec<u16>,
}

impl InitPacket {
    /// The init packet of the Feather nRF52840's Adafruit bootloader: device
    /// type 0x0052 and the firmware id 0x00B6 of its S140 6.1.1.
    pub fn feather() -> InitPacket {
        InitPacket {
            dev_type: 0x0052,
            dev_rev: 0xFFFF,
            app_ver: 0xFFFF_FFFF,
            sd_req: vec![0x00B6],
        }
    }

    /// Returns the init packet of an image with CRC crc, little endian: the
    /// device type and revision, the application version, the number of
    /// SoftDevice ids, the ids and the CRC.
    pub fn marshal(&self, crc: u16) -> Vec<u8> {
        let mut b = Vec::new();
        b.extend_from_slice(&self.dev_type.to_le_bytes());
        b.extend_from_slice(&self.dev_rev.to_le_bytes());
        b.extend_from_slice(&self.app_ver.to_le_bytes());
        b.extend_from_slice(&(self.sd_req.len() as u16).to_le_bytes());
        for sd in &self.sd_req {
            b.extend_from_slice(&sd.to_le_bytes());
        }
        b.extend_from_slice(&crc.to_le_bytes());
        b
    }
}

/// The CRC of the bootloader's crc16_compute, CRC-16/CCITT-FALSE:
/// polynomial 0x1021, initial value 0xFFFF.
pub fn crc16(data: &[u8]) -> u16 {
    let mut crc = 0xFFFFu16;
    for &b in data {
        crc = crc.rotate_left(8);
        crc ^= u16::from(b);
        crc ^= (crc & 0xFF) >> 4;
        crc ^= crc << 12;
        crc ^= (crc & 0xFF) << 5;
    }
    crc
}

impl Package {
    /// Returns the package of the application image bin with the init packet
    /// fields ip.
    pub fn new(bin: Vec<u8>, ip: &InitPacket) -> Package {
        let dat = ip.marshal(crc16(&bin));
        Package { bin, dat }
    }

    /// Writes the package as a zip to w: name.bin, name.dat and the
    /// manifest.json with the init packet fields ip, as adafruit-nrfutil dfu
    /// genpkg does.
    pub fn write_zip<W: Write + Seek>(
        &self,
        w: W,
        name: &str,
        ip: &InitPacket,
    ) -> zip::result::ZipResult<()> {
        let (bin_file, dat_file) = (format!("{name}.bin"), format!("{name}.dat"));
        let manifest = manifest(&bin_file, &dat_file, ip, crc16(&self.bin));
        let mut z = ZipWriter::new(w);
        let opts = SimpleFileOptions::default()
            .compression_method(CompressionMethod::Deflated)
            .last_modified_time(now());
        for (file, data) in [
            (&bin_file, &self.bin[..]),
            (&dat_file, &self.dat[..]),
            (&"manifest.json".to_string(), manifest.as_bytes()),
        ] {
            z.start_file(file.as_str(), opts)?;
            z.write_all(data)?;
        }
        z.finish()?;
        Ok(())
    }
}

/// The time now in UTC, for the files of a package.
fn now() -> zip::DateTime {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs());
    date_time(secs).unwrap_or_default()
}

/// Converts seconds since the Unix epoch to a zip date and time, which
/// starts in 1980.
fn date_time(secs: u64) -> Option<zip::DateTime> {
    let (days, rem) = ((secs / 86_400) as i64, secs % 86_400);
    // The civil date of a day count (Howard Hinnant's days_from_civil
    // inverted).
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    zip::DateTime::from_date_and_time(
        u16::try_from(y).ok()?,
        m as u8,
        d as u8,
        (rem / 3600) as u8,
        (rem / 60 % 60) as u8,
        (rem % 60) as u8,
    )
    .ok()
}

/// The manifest.json of a genpkg package, as adafruit-nrfutil writes it:
/// the keys sorted, indented by 4 spaces, no newline at the end.
fn manifest(bin_file: &str, dat_file: &str, ip: &InitPacket, crc: u16) -> String {
    let sds: Vec<String> = ip
        .sd_req
        .iter()
        .map(|sd| format!("                    {sd}"))
        .collect();
    let sd_list = if sds.is_empty() {
        "[]".to_string()
    } else {
        format!("[\n{}\n                ]", sds.join(",\n"))
    };
    format!(
        r#"{{
    "manifest": {{
        "application": {{
            "bin_file": {bin},
            "dat_file": {dat},
            "init_packet_data": {{
                "application_version": {app_ver},
                "device_revision": {dev_rev},
                "device_type": {dev_type},
                "firmware_crc16": {crc},
                "softdevice_req": {sd_list}
            }}
        }},
        "dfu_version": 0.5
    }}
}}"#,
        bin = serde_json::to_string(bin_file).unwrap(),
        dat = serde_json::to_string(dat_file).unwrap(),
        app_ver = ip.app_ver,
        dev_rev = ip.dev_rev,
        dev_type = ip.dev_type,
    )
}

/// Parses a comma separated list of 16-bit numbers, such as 0x00B6.
pub fn parse_ids(s: &str) -> Result<Vec<u16>, String> {
    s.split(',')
        .map(|f| parse_uint(f.trim(), 16).map(|v| v as u16))
        .collect()
}

/// Parses a number of at most bits bits, decimal or with a 0x, 0o or 0b
/// prefix, as Go's strconv.ParseUint with base 0.
pub fn parse_uint(s: &str, bits: u32) -> Result<u64, String> {
    let (digits, radix) = match s.get(..2) {
        Some("0x" | "0X") => (&s[2..], 16),
        Some("0o" | "0O") => (&s[2..], 8),
        Some("0b" | "0B") => (&s[2..], 2),
        _ => (s, 10),
    };
    let v = u64::from_str_radix(digits, radix).map_err(|e| format!("{s:?}: {e}"))?;
    if bits < 64 && v >> bits != 0 {
        return Err(format!("{s:?}: more than {bits} bits"));
    }
    Ok(v)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    #[test]
    fn crc() {
        assert_eq!(crc16(b"123456789"), 0x29B1);
    }

    #[test]
    fn init_packet() {
        // The knixx.dat of a package built by adafruit-nrfutil.
        let want = [
            0x52, 0x00, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0x01, 0x00, 0xb6, 0x00, 0xea, 0x9a,
        ];
        assert_eq!(InitPacket::feather().marshal(0x9AEA), want);
    }

    #[test]
    fn dates() {
        let d = date_time(1_793_134_393).unwrap(); // 2026-10-27T20:53:13Z
        assert_eq!(
            (d.year(), d.month(), d.day(), d.hour(), d.minute(), d.second()),
            (2026, 10, 27, 20, 53, 12) // zip keeps even seconds
        );
        let d = date_time(951_782_400).unwrap(); // 2000-02-29
        assert_eq!((d.year(), d.month(), d.day()), (2000, 2, 29));
        assert!(date_time(0).is_none(), "before 1980");
    }

    #[test]
    fn ids() {
        assert_eq!(parse_ids("0x00B6"), Ok(vec![0xB6]));
        assert_eq!(parse_ids("0xB6, 0x00A8,182"), Ok(vec![0xB6, 0xA8, 182]));
        assert!(parse_ids("0x10000").is_err());
        assert!(parse_ids("").is_err());
        assert!(parse_ids("b6").is_err());
    }

    #[test]
    fn write_zip() {
        let ip = InitPacket::feather();
        let p = Package::new(vec![1, 2, 3, 4, 5, 6, 7, 8], &ip);
        let mut buf = Cursor::new(Vec::new());
        p.write_zip(&mut buf, "knixx", &ip).unwrap();
        let data = buf.into_inner();
        let got = Package::from_zip(Cursor::new(&data)).unwrap();
        assert_eq!(got, p);

        // The manifest as adafruit-nrfutil writes it.
        let mut z = zip::ZipArchive::new(Cursor::new(&data)).unwrap();
        let raw = std::io::read_to_string(z.by_name("manifest.json").unwrap()).unwrap();
        let want = format!(
            r#"{{
    "manifest": {{
        "application": {{
            "bin_file": "knixx.bin",
            "dat_file": "knixx.dat",
            "init_packet_data": {{
                "application_version": 4294967295,
                "device_revision": 65535,
                "device_type": 82,
                "firmware_crc16": {},
                "softdevice_req": [
                    182
                ]
            }}
        }},
        "dfu_version": 0.5
    }}
}}"#,
            crc16(&p.bin)
        );
        assert_eq!(raw, want);
        let _: serde_json::Value = serde_json::from_str(&raw).unwrap();
        // The file order of adafruit-nrfutil, deflated.
        let names: Vec<_> = (0..z.len())
            .map(|i| z.by_index(i).unwrap().name().unwrap().to_string())
            .collect();
        assert_eq!(names, ["knixx.bin", "knixx.dat", "manifest.json"]);
        assert_eq!(z.by_index(0).unwrap().compression(), CompressionMethod::Deflated);
    }

    #[test]
    fn manifest_sd_list() {
        let mut ip = InitPacket::feather();
        ip.sd_req = vec![0xB6, 0xA8];
        let m = manifest("a.bin", "a.dat", &ip, 1);
        assert!(m.contains(
            "\"softdevice_req\": [\n                    182,\n                    168\n                ]"
        ));
        let _: serde_json::Value = serde_json::from_str(&m).unwrap();
    }

    /// The package of the firmware, when cargo xtask build made its hex
    /// file.
    #[test]
    fn build_hex() {
        let path = concat!(env!("CARGO_MANIFEST_DIR"), "/../../build/knixx.hex");
        let Ok(text) = std::fs::read_to_string(path) else {
            return;
        };
        let (base, bin) = super::super::read_hex(&text).unwrap();
        assert_eq!(base, 0x26000, "the application follows the S140");
        let ip = InitPacket::feather();
        let mut buf = Cursor::new(Vec::new());
        Package::new(bin, &ip).write_zip(&mut buf, "knixx", &ip).unwrap();
        Package::from_zip(Cursor::new(buf.into_inner())).unwrap();
    }
}
