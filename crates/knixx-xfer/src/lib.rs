//! The one transfer protocol of knixx thunder: a file, firmware, a script
//! or the config, goes to a board (a push) or comes from it (a pull), over
//! the app's BLE transfer service or the bootloader's USB recovery.
//!
//! The host starts every transfer with a request: `put` offers a file,
//! `get` asks for one. From then on the end with the file is the sender
//! and the other the receiver: the sender sends `data` at offsets, the
//! receiver takes only the offset it expects and answers `ack` every 4 KiB
//! (and at the end), `rewind` for data at another offset, and `done` or
//! `error` once it has checked the file. The last `ack` goes out before the
//! check, so that a sender is not left waiting through it.
//!
//! - [`msg`]: the messages and their encoding.
//! - [`frame`]: the framing of messages on a byte stream (USB).
//! - [`receiver`]: the receiving end, over a [`receiver::Sink`].
//! - [`sender`]: the sending end's window, acks, rewinds and stalls.
//!
//! The ends are pure state machines: the board's BLE task, the bootloader's
//! USB loop and bolt do the I/O and give the time.

#![no_std]

#[cfg(test)]
extern crate std;

pub mod frame;
pub mod msg;
pub mod receiver;
pub mod sender;

#[cfg(test)]
mod tests;

pub use msg::{Kind, Msg};

/// The largest script: Rhai Grain bytecode, as a script's file on the board.
pub const MAX_SCRIPT: u32 = 16 * 1024;
/// The largest config: the board's knixx.ini.
pub const MAX_CONFIG: u32 = knixx_device::config::MAX_FILE as u32;
/// The largest script list.
pub const MAX_LIST: u32 = 2048;
/// The largest firmware image.
pub const MAX_FIRMWARE: u32 = knixx_image::MAX_IMAGE;

/// The receiver acks every ACK_EVERY bytes it takes, and at the end.
pub const ACK_EVERY: u32 = 4096;

/// The longest name, as a script's.
pub const MAX_NAME: usize = knixx_console::upload::MAX_NAME;

/// The cap of a kind.
pub fn cap(kind: Kind) -> u32 {
    match kind {
        Kind::Firmware => MAX_FIRMWARE,
        Kind::Script => MAX_SCRIPT,
        Kind::Config => MAX_CONFIG,
        Kind::ScriptList => MAX_LIST,
    }
}

/// Error codes of `error`. Below 0x40: an image's refusal
/// (knixx_image::Refusal::code).
pub mod code {
    /// The board is not in an update window.
    pub const WINDOW: u8 = 0x40;
    /// Another transfer is in progress.
    pub const BUSY: u8 = 0x41;
    /// This place does not take that kind (or that direction).
    pub const KIND: u8 = 0x42;
    /// Size 0, or over the kind's cap.
    pub const SIZE: u8 = 0x43;
    /// A bad name.
    pub const NAME: u8 = 0x44;
    /// No such file, to a get.
    pub const NOT_FOUND: u8 = 0x45;
    /// The flash failed.
    pub const STORAGE: u8 = 0x46;
    /// The file's CRC does not match.
    pub const CRC: u8 = 0x47;
    /// Not enough room on the flash.
    pub const SPACE: u8 = 0x48;
    /// A config that does not parse or does not check.
    pub const CONFIG: u8 = 0x49;
    /// A config for another individual address, without force.
    pub const GUARD: u8 = 0x4A;
    /// A message out of step.
    pub const STATE: u8 = 0x4B;
    /// The other end gave up.
    pub const ABORTED: u8 = 0x4C;
}

/// The CRC-32 of data, as the protocol uses it: zlib's.
pub fn crc32(data: &[u8]) -> u32 {
    knixx_image::crc32(data)
}

/// Whether name is valid for kind: scripts need a script name, the others
/// none.
pub fn valid_name(kind: Kind, name: &str) -> bool {
    match kind {
        Kind::Script => knixx_console::upload::valid_name(name),
        _ => name.is_empty(),
    }
}
