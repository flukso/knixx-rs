//! The board's 2 MiB QSPI flash (GD25Q16C), driven in single-line SPI mode
//! at 8 MHz, as a knixx_storage block device: the firmware's littlefs, and
//! the bootloader's read of the staged image on it.
//!
//! QSPI's EasyDMA reads from and writes to word-aligned RAM only, while
//! littlefs passes any buffer, also data in flash such as the default
//! configuration, so all transfers go through an aligned bounce buffer.

#![no_std]

use embassy_nrf::interrupt::typelevel::Binding;
use embassy_nrf::peripherals::{P0_17, P0_19, P0_20, P0_21, P0_22, P0_23, QSPI};
use embassy_nrf::qspi::{self, AddressMode, Frequency, Qspi, ReadOpcode, WriteOpcode, WritePageSize};
use embassy_nrf::{Peri, interrupt};
use knixx_storage::{BlockDevice, PAGE_SIZE, SECTOR_COUNT, SECTOR_SIZE};

/// The QSPI flash as a knixx_storage block device.
pub struct QspiFlash {
    qspi: Qspi<'static>,
    bounce: Bounce,
}

#[repr(C, align(4))]
struct Bounce([u8; PAGE_SIZE]);

/// A QSPI transfer failed.
pub struct FlashError;

/// The pins of the flash: SCK, CSN, IO0..IO3 (WP# and HOLD#, held high).
pub struct Pins {
    pub sck: Peri<'static, P0_19>,
    pub csn: Peri<'static, P0_20>,
    pub io0: Peri<'static, P0_17>,
    pub io1: Peri<'static, P0_22>,
    pub io2: Peri<'static, P0_23>,
    pub io3: Peri<'static, P0_21>,
}

impl QspiFlash {
    /// Sets up the flash and wakes it, in case it was left in deep
    /// power-down.
    pub fn new(
        qspi: Peri<'static, QSPI>,
        irq: impl Binding<interrupt::typelevel::QSPI, qspi::InterruptHandler<QSPI>> + 'static,
        pins: Pins,
    ) -> QspiFlash {
        let mut cfg = qspi::Config::default();
        cfg.read_opcode = ReadOpcode::Fastread;
        cfg.write_opcode = WriteOpcode::Pp;
        cfg.write_page_size = WritePageSize::_256bytes;
        cfg.address_mode = AddressMode::_24bit;
        cfg.frequency = Frequency::M8;
        cfg.capacity = (SECTOR_SIZE * SECTOR_COUNT) as u32;
        let qspi = Qspi::new(
            qspi, irq, pins.sck, pins.csn, pins.io0, pins.io1, pins.io2, pins.io3, cfg,
        );
        let mut flash = QspiFlash {
            qspi,
            bounce: Bounce([0; PAGE_SIZE]),
        };
        let _ = flash.qspi.blocking_custom_instruction(0xAB, &[], &mut []);
        flash
    }
}

impl BlockDevice for QspiFlash {
    type Error = FlashError;

    fn read(&mut self, off: u32, buf: &mut [u8]) -> Result<(), FlashError> {
        let mut off = off;
        for chunk in buf.chunks_mut(PAGE_SIZE) {
            // QSPI reads whole words.
            let n = chunk.len().next_multiple_of(4);
            self.qspi
                .blocking_read(off, &mut self.bounce.0[..n])
                .map_err(|_| FlashError)?;
            chunk.copy_from_slice(&self.bounce.0[..chunk.len()]);
            off += chunk.len() as u32;
        }
        Ok(())
    }

    fn write(&mut self, off: u32, data: &[u8]) -> Result<(), FlashError> {
        let mut off = off;
        for chunk in data.chunks(PAGE_SIZE) {
            self.bounce.0[..chunk.len()].copy_from_slice(chunk);
            self.qspi
                .blocking_write(off, &self.bounce.0[..chunk.len()])
                .map_err(|_| FlashError)?;
            off += chunk.len() as u32;
        }
        Ok(())
    }

    fn erase_sector(&mut self, off: u32) -> Result<(), FlashError> {
        self.qspi.blocking_erase(off).map_err(|_| FlashError)
    }
}
