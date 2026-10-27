//! A firmware image: the bytes that go to flash, by address, read from the
//! loadable segments of an ELF file and written as a raw binary, Intel HEX
//! or UF2.

use std::collections::BTreeMap;
use std::fmt::Write as _;

use object::{Object, ObjectSection};

use crate::uf2;

/// An error reading or converting an image.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("elf: {0}")]
    Elf(#[from] object::Error),
    #[error("no loadable data in the ELF file")]
    Empty,
    #[error("segments overlap at {0:#x}")]
    Overlap(u64),
    #[error("address {0:#x} is beyond 32 bits")]
    Address(u64),
}

/// The bytes of an image by address, in contiguous runs that do not touch.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Image {
    runs: BTreeMap<u32, Vec<u8>>,
}

impl Image {
    /// Returns the image of an ELF file: the bytes of its allocated
    /// sections that have data in the file, at their load addresses. .data
    /// lies in RAM but loads into flash after .rodata, where the startup
    /// code copies it from. Like objcopy, it works from the sections, not
    /// the segments: a linker may map the ELF headers into a loadable
    /// segment below the image.
    pub fn from_elf(data: &[u8]) -> Result<Image, Error> {
        let file = object::File::parse(data)?;
        let loads = load_segments(&file);
        let mut img = Image::default();
        for sec in file.sections() {
            let object::SectionFlags::Elf { sh_flags } = sec.flags() else {
                continue;
            };
            if sh_flags & u64::from(object::elf::SHF_ALLOC) == 0 || sec.file_range().is_none() {
                continue;
            }
            let bytes = sec.data()?;
            if bytes.is_empty() {
                continue;
            }
            let vaddr = sec.address();
            let lma = loads
                .iter()
                .find(|l| vaddr >= l.vaddr && vaddr < l.vaddr + l.memsz)
                .map_or(vaddr, |l| vaddr - l.vaddr + l.paddr);
            img.add(lma, bytes)?;
        }
        if img.runs.is_empty() {
            return Err(Error::Empty);
        }
        Ok(img)
    }

    /// Adds bytes at addr, merging them with a run they touch.
    pub fn add(&mut self, addr: u64, bytes: &[u8]) -> Result<(), Error> {
        let end = addr + bytes.len() as u64;
        if end > u64::from(u32::MAX) + 1 {
            return Err(Error::Address(end));
        }
        let addr = addr as u32;
        let end = end as u32;
        if let Some((&a, run)) = self.runs.range(..end).next_back()
            && a + run.len() as u32 > addr
        {
            return Err(Error::Overlap(u64::from(addr.max(a))));
        }
        let mut data = bytes.to_vec();
        let mut start = addr;
        // Merge with the run ending at addr and the one starting at end.
        if let Some((&a, run)) = self.runs.range(..addr).next_back()
            && a + run.len() as u32 == addr
        {
            let mut prev = self.runs.remove(&a).unwrap();
            prev.extend_from_slice(&data);
            data = prev;
            start = a;
        }
        if let Some(next) = self.runs.remove(&end) {
            data.extend_from_slice(&next);
        }
        self.runs.insert(start, data);
        Ok(())
    }

    /// Returns the runs of bytes by address.
    pub fn runs(&self) -> impl Iterator<Item = (u32, &[u8])> {
        self.runs.iter().map(|(&a, d)| (a, d.as_slice()))
    }

    /// Returns the lowest address.
    pub fn start(&self) -> u32 {
        *self.runs.keys().next().unwrap_or(&0)
    }

    /// Returns the address past the last byte.
    pub fn end(&self) -> u32 {
        self.runs
            .iter()
            .next_back()
            .map_or(0, |(&a, d)| a + d.len() as u32)
    }

    /// Returns the image as one block from start to end, with 0xFF, the
    /// value of erased flash, in the gaps.
    pub fn to_bin(&self) -> Vec<u8> {
        let start = self.start();
        let mut out = vec![0xFF; (self.end() - start) as usize];
        for (a, d) in self.runs() {
            let off = (a - start) as usize;
            out[off..off + d.len()].copy_from_slice(d);
        }
        out
    }

    /// Returns the image as Intel HEX: 16 data bytes per record, an extended
    /// linear address record whenever the upper 16 bits change.
    pub fn to_hex(&self) -> String {
        let mut out = String::new();
        let mut upper = None;
        for (a, d) in self.runs() {
            let mut addr = a;
            for chunk in chunks_within_64k(addr, d) {
                let hi = (addr >> 16) as u16;
                if upper != Some(hi) {
                    hex_record(&mut out, 0, 4, &hi.to_be_bytes());
                    upper = Some(hi);
                }
                hex_record(&mut out, addr as u16, 0, chunk);
                addr += chunk.len() as u32;
            }
        }
        hex_record(&mut out, 0, 1, &[]);
        out
    }

    /// Returns the image as UF2 for family, in 256-byte blocks aligned to 256
    /// bytes. Only the 256-byte pages that hold image bytes are written; the
    /// gaps within a page are 0xFF.
    pub fn to_uf2(&self, family: u32) -> Vec<u8> {
        let mut pages: BTreeMap<u32, [u8; uf2::PAYLOAD]> = BTreeMap::new();
        for (a, d) in self.runs() {
            for (i, &b) in d.iter().enumerate() {
                let addr = a + i as u32;
                let page = addr & !(uf2::PAYLOAD as u32 - 1);
                pages.entry(page).or_insert([0xFF; uf2::PAYLOAD])[(addr - page) as usize] = b;
            }
        }
        let n = pages.len() as u32;
        let mut out = Vec::with_capacity(pages.len() * uf2::BLOCK_SIZE);
        for (i, (addr, payload)) in pages.iter().enumerate() {
            out.extend_from_slice(&uf2::block(*addr, payload, i as u32, n, family));
        }
        out
    }
}

/// A loadable ELF segment: where it runs and where it loads.
struct Load {
    vaddr: u64,
    paddr: u64,
    memsz: u64,
}

/// Returns the loadable segments of an ELF file with file data.
fn load_segments(file: &object::File) -> Vec<Load> {
    use object::read::elf::ProgramHeader;
    let object::File::Elf32(f) = file else {
        return Vec::new();
    };
    let endian = f.endian();
    f.elf_program_headers()
        .iter()
        .filter(|ph| ph.p_type(endian) == object::elf::PT_LOAD && ph.p_filesz(endian) > 0)
        .map(|ph| Load {
            vaddr: u64::from(ph.p_vaddr(endian)),
            paddr: u64::from(ph.p_paddr(endian)),
            memsz: u64::from(ph.p_memsz(endian)),
        })
        .collect()
}

/// Splits d, at addr, into chunks of at most 16 bytes that do not cross a
/// 64 KiB boundary.
fn chunks_within_64k(addr: u32, d: &[u8]) -> Vec<&[u8]> {
    let mut out = Vec::new();
    let mut a = addr;
    let mut rest = d;
    while !rest.is_empty() {
        let to_boundary = 0x1_0000 - (a & 0xFFFF);
        let n = rest.len().min(16).min(to_boundary as usize);
        out.push(&rest[..n]);
        rest = &rest[n..];
        a += n as u32;
    }
    out
}

/// Appends an Intel HEX record.
fn hex_record(out: &mut String, addr: u16, kind: u8, data: &[u8]) {
    let mut sum = data.len() as u8;
    sum = sum
        .wrapping_add((addr >> 8) as u8)
        .wrapping_add(addr as u8)
        .wrapping_add(kind);
    write!(out, ":{:02X}{:04X}{:02X}", data.len(), addr, kind).unwrap();
    for &b in data {
        write!(out, "{b:02X}").unwrap();
        sum = sum.wrapping_add(b);
    }
    writeln!(out, "{:02X}", sum.wrapping_neg()).unwrap();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn add_merges_touching_runs() {
        let mut img = Image::default();
        img.add(0x100, &[1, 2]).unwrap();
        img.add(0x104, &[5]).unwrap();
        img.add(0x102, &[3, 4]).unwrap();
        let runs: Vec<_> = img.runs().collect();
        assert_eq!(runs, vec![(0x100, &[1u8, 2, 3, 4, 5][..])]);
    }

    #[test]
    fn add_rejects_overlap() {
        let mut img = Image::default();
        img.add(0x100, &[1, 2, 3]).unwrap();
        assert!(matches!(img.add(0x102, &[9]), Err(Error::Overlap(0x102))));
    }

    #[test]
    fn bin_fills_gaps() {
        let mut img = Image::default();
        img.add(0x10, &[1]).unwrap();
        img.add(0x13, &[2]).unwrap();
        assert_eq!(img.to_bin(), vec![1, 0xFF, 0xFF, 2]);
    }

    #[test]
    fn hex_records() {
        let mut img = Image::default();
        img.add(0x2_6000, &[0x00, 0x01, 0x02, 0x03]).unwrap();
        // Checksums from the Intel HEX definition: two's complement of the
        // byte sum.
        assert_eq!(
            img.to_hex(),
            ":020000040002F8\n:046000000001020396\n:00000001FF\n"
        );
    }

    #[test]
    fn hex_splits_at_64k() {
        let mut img = Image::default();
        img.add(0x1_FFFE, &[1, 2, 3, 4]).unwrap();
        let hex = img.to_hex();
        let lines: Vec<_> = hex.lines().collect();
        assert_eq!(lines[0], ":020000040001F9");
        assert!(lines[1].starts_with(":02FFFE00"));
        assert_eq!(lines[2], ":020000040002F8");
        assert!(lines[3].starts_with(":02000000"));
    }

    #[test]
    fn uf2_pages() {
        let mut img = Image::default();
        img.add(0x2_6000, &[0xAA; 300]).unwrap();
        img.add(0x2_7010, &[0xBB]).unwrap();
        let f = img.to_uf2(uf2::FAMILY_NRF52840);
        assert_eq!(f.len(), 3 * uf2::BLOCK_SIZE);
        uf2::check(&f, uf2::FAMILY_NRF52840).unwrap();
        let blocks: Vec<_> = uf2::blocks(&f).collect();
        assert_eq!(blocks[0].addr, 0x2_6000);
        assert_eq!(blocks[1].addr, 0x2_6100);
        assert_eq!(blocks[1].payload[43], 0xAA);
        assert_eq!(blocks[1].payload[44], 0xFF);
        assert_eq!(blocks[2].addr, 0x2_7000);
        assert_eq!(blocks[2].payload[0x10], 0xBB);
    }
}
