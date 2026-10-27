//! UF2 files, as the Adafruit nRF52 UF2 bootloader takes them on its USB
//! drive. <https://github.com/microsoft/uf2>

/// The size of a UF2 block; every block is one sector.
pub const BLOCK_SIZE: usize = 512;

/// The payload of each block written here.
pub const PAYLOAD: usize = 256;

/// The UF2 family of the nRF52840 that the Adafruit bootloader expects.
pub const FAMILY_NRF52840: u32 = 0xADA5_2840;

/// The file the bootloader's drive holds, with its board ID.
pub const INFO_FILE: &str = "INFO_UF2.TXT";

const MAGIC_START0: u32 = 0x0A32_4655;
const MAGIC_START1: u32 = 0x9E5D_5157;
const MAGIC_END: u32 = 0x0AB1_6F30;

const FLAG_NOT_MAIN_FLASH: u32 = 0x0000_0001;
const FLAG_FAMILY_ID: u32 = 0x0000_2000;

/// An error in a UF2 file.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum Error {
    #[error("uf2: {0} bytes, not whole {BLOCK_SIZE}-byte blocks")]
    Size(usize),
    #[error("uf2: block {0}: not a UF2 block")]
    Magic(u32),
    #[error("uf2: block {block} is numbered {no} of {total}, want {block} of {want}")]
    Number {
        block: u32,
        no: u32,
        total: u32,
        want: u32,
    },
    #[error("uf2: block {0} has no family ID")]
    NoFamily(u32),
    #[error("uf2: block {block} is for family {family:#010x}, want {want:#010x}")]
    Family { block: u32, family: u32, want: u32 },
}

/// Returns the UF2 block n of total for payload at addr, for family.
pub fn block(addr: u32, payload: &[u8], n: u32, total: u32, family: u32) -> [u8; BLOCK_SIZE] {
    assert!(payload.len() <= 476);
    let mut b = [0u8; BLOCK_SIZE];
    let words = [
        MAGIC_START0,
        MAGIC_START1,
        FLAG_FAMILY_ID,
        addr,
        payload.len() as u32,
        n,
        total,
        family,
    ];
    for (i, w) in words.iter().enumerate() {
        b[4 * i..4 * i + 4].copy_from_slice(&w.to_le_bytes());
    }
    b[32..32 + payload.len()].copy_from_slice(payload);
    b[508..].copy_from_slice(&MAGIC_END.to_le_bytes());
    b
}

/// A block of a UF2 file.
#[derive(Debug, PartialEq, Eq)]
pub struct Block<'a> {
    pub flags: u32,
    pub addr: u32,
    pub payload: &'a [u8],
}

/// Returns the blocks of a UF2 file; see [`check`] first.
pub fn blocks(data: &[u8]) -> impl Iterator<Item = Block<'_>> {
    data.as_chunks::<BLOCK_SIZE>().0.iter().map(|b| {
        let size = (word(b, 16) as usize).min(476);
        Block {
            flags: word(b, 8),
            addr: word(b, 12),
            payload: &b[32..32 + size],
        }
    })
}

fn word(b: &[u8], off: usize) -> u32 {
    u32::from_le_bytes(b[off..off + 4].try_into().unwrap())
}

/// Checks that data is a complete UF2 file for family: whole blocks with the
/// UF2 magic numbers, the family ID in every block that goes to the main
/// flash, and the block numbers 0 to n-1 of n blocks.
pub fn check(data: &[u8], family: u32) -> Result<(), Error> {
    if data.is_empty() || !data.len().is_multiple_of(BLOCK_SIZE) {
        return Err(Error::Size(data.len()));
    }
    let n = (data.len() / BLOCK_SIZE) as u32;
    for (i, b) in data.as_chunks::<BLOCK_SIZE>().0.iter().enumerate() {
        let i = i as u32;
        if word(b, 0) != MAGIC_START0 || word(b, 4) != MAGIC_START1 || word(b, 508) != MAGIC_END {
            return Err(Error::Magic(i));
        }
        if word(b, 20) != i || word(b, 24) != n {
            return Err(Error::Number {
                block: i,
                no: word(b, 20),
                total: word(b, 24),
                want: n,
            });
        }
        let flags = word(b, 8);
        if flags & FLAG_NOT_MAIN_FLASH != 0 {
            continue;
        }
        if flags & FLAG_FAMILY_ID == 0 {
            return Err(Error::NoFamily(i));
        }
        if word(b, 28) != family {
            return Err(Error::Family {
                block: i,
                family: word(b, 28),
                want: family,
            });
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn file(n: u32) -> Vec<u8> {
        (0..n)
            .flat_map(|i| block(0x26000 + 256 * i, &[0; 256], i, n, FAMILY_NRF52840))
            .collect()
    }

    #[test]
    fn check_good() {
        assert_eq!(check(&file(3), FAMILY_NRF52840), Ok(()));
    }

    #[test]
    fn check_size() {
        assert_eq!(check(&[], FAMILY_NRF52840), Err(Error::Size(0)));
        assert_eq!(check(&file(1)[..511], FAMILY_NRF52840), Err(Error::Size(511)));
    }

    #[test]
    fn check_magic() {
        let mut f = file(2);
        f[512 + 508] ^= 1;
        assert_eq!(check(&f, FAMILY_NRF52840), Err(Error::Magic(1)));
    }

    #[test]
    fn check_numbering() {
        let mut f = file(2);
        f.truncate(512);
        assert!(matches!(
            check(&f, FAMILY_NRF52840),
            Err(Error::Number {
                block: 0,
                want: 1,
                ..
            })
        ));
    }

    #[test]
    fn check_family() {
        assert!(matches!(
            check(&file(1), 0xE48B_FF56),
            Err(Error::Family { block: 0, .. })
        ));
        let mut f = file(1);
        f[8..12].copy_from_slice(&0u32.to_le_bytes());
        assert_eq!(check(&f, FAMILY_NRF52840), Err(Error::NoFamily(0)));
        f[8..12].copy_from_slice(&FLAG_NOT_MAIN_FLASH.to_le_bytes());
        assert_eq!(check(&f, FAMILY_NRF52840), Ok(()));
    }
}
