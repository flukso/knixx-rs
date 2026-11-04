//! Writes an image into internal flash: the body first, page by page from
//! APP_START, and the header last, so that a reset halfway leaves an
//! application without a header, which the next boot does not start.

use embassy_nrf::nvmc::Nvmc;
use embedded_storage::nor_flash::NorFlash;
use knixx_image::{self as image, HEADER_SIZE};

/// Writing internal flash failed.
#[derive(Debug)]
pub struct WriteError;

/// An image being written into internal flash.
pub struct Writer<'a> {
    nvmc: &'a mut Nvmc<'static>,
    /// The address up to which pages are erased.
    erased: u32,
    /// The bytes written of the body.
    written: u32,
    /// A tail of fewer than 4 bytes, waiting for the next piece: the NVMC
    /// writes whole words.
    tail: [u8; 4],
    tail_len: usize,
}

impl<'a> Writer<'a> {
    /// Erases the header's page, which makes the installed application
    /// invalid, and starts the body at APP_START.
    pub fn begin(nvmc: &'a mut Nvmc<'static>) -> Result<Writer<'a>, WriteError> {
        let first = image::HEADER_ADDR;
        nvmc.erase(first, first + image::FLASH_PAGE)
            .map_err(|_| WriteError)?;
        Ok(Writer {
            nvmc,
            erased: first + image::FLASH_PAGE,
            written: 0,
            tail: [0xFF; 4],
            tail_len: 0,
        })
    }

    /// Writes the next piece of the body.
    pub fn write(&mut self, mut data: &[u8]) -> Result<(), WriteError> {
        if self.written as usize + self.tail_len + data.len() > image::MAX_BODY as usize {
            return Err(WriteError);
        }
        if self.tail_len > 0 {
            let n = (4 - self.tail_len).min(data.len());
            self.tail[self.tail_len..self.tail_len + n].copy_from_slice(&data[..n]);
            self.tail_len += n;
            data = &data[n..];
            if self.tail_len < 4 {
                return Ok(());
            }
            let word = self.tail;
            self.raw(&word)?;
            self.tail_len = 0;
        }
        let whole = data.len() / 4 * 4;
        if whole > 0 {
            self.raw(&data[..whole])?;
        }
        let rest = &data[whole..];
        self.tail[..rest.len()].copy_from_slice(rest);
        self.tail_len = rest.len();
        Ok(())
    }

    /// Writes whole words at the end of the body, erasing pages ahead.
    fn raw(&mut self, words: &[u8]) -> Result<(), WriteError> {
        let addr = image::APP_START + self.written;
        let end = addr + words.len() as u32;
        while self.erased < end {
            self.nvmc
                .erase(self.erased, self.erased + image::FLASH_PAGE)
                .map_err(|_| WriteError)?;
            self.erased += image::FLASH_PAGE;
        }
        self.nvmc.write(addr, words).map_err(|_| WriteError)?;
        self.written += words.len() as u32;
        Ok(())
    }

    /// Writes the last partial word, padded with 0xFF, and returns the
    /// body's length.
    pub fn flush(&mut self) -> Result<u32, WriteError> {
        let len = self.written + self.tail_len as u32;
        if self.tail_len > 0 {
            let mut word = [0xFF; 4];
            word[..self.tail_len].copy_from_slice(&self.tail[..self.tail_len]);
            self.raw(&word)?;
            self.tail_len = 0;
        }
        Ok(len)
    }

    /// Writes the header, which makes the application valid.
    pub fn commit(self, header: &[u8; HEADER_SIZE]) -> Result<(), WriteError> {
        self.nvmc
            .write(image::HEADER_ADDR, header)
            .map_err(|_| WriteError)
    }
}
