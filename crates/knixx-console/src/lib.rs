//! The console's line editor, output queues, BLE NUS framing and uploads
#![no_std]

#[cfg(test)]
extern crate std;

pub mod lineedit;
pub mod nus;
pub mod outq;
pub mod name;
