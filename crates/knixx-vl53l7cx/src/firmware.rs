//! The arrays of the ULD's vl53l7cx_buffers.h
//! (`VL53L7CX_ULD_driver_2.0.1/VL53L7CX_ULD_API/inc/`), extracted into
//! firmware/. They stay in flash.

pub(crate) static FIRMWARE: &[u8] = include_bytes!("../firmware/firmware.bin");
pub(crate) static DEFAULT_CONFIGURATION: &[u8] = include_bytes!("../firmware/default_configuration.bin");
pub(crate) static DEFAULT_XTALK: &[u8; 776] = include_bytes!("../firmware/default_xtalk.bin");
pub(crate) static GET_NVM_CMD: &[u8] = include_bytes!("../firmware/get_nvm_cmd.bin");
