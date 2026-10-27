//! The console's line editor, output queues and BLE NUS framing
#![no_std]

#[cfg(test)]
extern crate std;

pub mod lineedit;
pub mod nus;
pub mod outq;
