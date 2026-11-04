//! The installed application: its header in flash, its check, and the jump
//! to it.

use embassy_nrf::pac;
use knixx_image::{self as image, HEADER_SIZE, Header};

/// The header of the installed application, if it is valid and its body's
/// CRC matches.
pub fn header() -> Option<Header> {
    // Safety: internal flash, memory-mapped and readable.
    let raw: &[u8; HEADER_SIZE] = unsafe { &*(image::HEADER_ADDR as *const [u8; HEADER_SIZE]) };
    let h = Header::parse(raw).ok()?;
    (image::crc32(body(h.len)) == h.body_crc).then_some(h)
}

/// The application's body of len bytes, in internal flash.
pub fn body(len: u32) -> &'static [u8] {
    let len = len.min(image::MAX_BODY) as usize;
    // Safety: within the application's region of internal flash.
    unsafe { core::slice::from_raw_parts(image::APP_START as *const u8, len) }
}

/// Write-protects the bootloader with ACL region 0, until the next reset,
/// and jumps to the application.
pub fn boot() -> ! {
    let acl = pac::ACL.acl(0);
    acl.addr().write_value(image::BOOT_START);
    acl.size().write_value(image::BOOT_SIZE);
    acl.perm().write(|w| w.set_write(pac::acl::vals::Write::Disable));
    // Safety: the header said a valid application starts at APP_START, with
    // its vector table there (cortex-m-rt's set-vtor sets VTOR again).
    unsafe {
        (*cortex_m::peripheral::SCB::PTR).vtor.write(image::APP_START);
        cortex_m::asm::bootload(image::APP_START as *const u32)
    }
}
