//! The motion indicator and detection thresholds plugins.

use embedded_hal_async::delay::DelayNs;
use embedded_hal_async::i2c::I2c;

use crate::{DCI_MOTION_CONFIG, Device, Error, MAX_MOTION_THRESHOLD, RESOLUTION_4X4, RESOLUTION_8X8, put32};

// Firmware data indexes of the motion indicator and detection thresholds
// plugins.
const DCI_DET_THRESH_CONFIG: u16 = 0x5488;
const DCI_DET_THRESH_GLOBAL_CONFIG: u16 = 0xB6E0;
const DCI_DET_THRESH_START: u16 = 0xB6E8;
const DCI_DET_THRESH_VALID_STATUS: u16 = 0xB9F0;

const NB_THRESHOLDS: usize = 64;
const THRESHOLD_SIZE: usize = 12;
const LAST_THRESHOLD: u8 = 128;
const MEASURE_MOTION: u8 = 19;
const GREATER_THAN_MAX: u8 = 3;

/// The size of VL53L7CX_Motion_Configuration.
const MOTION_CONFIG_SIZE: usize = 156;

/// The nearest edge of the motion indicator's distance window, in mm.
pub const MIN_MOTION_DISTANCE: u16 = 400;
/// The farthest edge of the motion indicator's distance window, in mm.
pub const MAX_MOTION_DISTANCE: u16 = 4000;
/// The widest distance window, in mm.
pub const MAX_MOTION_SPAN: u16 = 1500;

/// Reports whether set_motion_distance accepts the window from min to max
/// mm.
pub fn valid_motion_distance(min: u16, max: u16) -> bool {
    min >= MIN_MOTION_DISTANCE && max <= MAX_MOTION_DISTANCE && max > min && max - min <= MAX_MOTION_SPAN
}

/// Computes the ref_bin_offset and feature_length of the window from min to
/// max mm like the ULD, in single precision; f32 arithmetic rounds each step
/// as C's float_t does, and Rust never fuses it.
pub(crate) fn motion_fields(min: u16, max: u16) -> (i32, u8) {
    let mut t = f32::from(min) / 37.5348f32;
    t -= 4.0f32;
    t *= 2048.5f32;
    let ref_bin_offset = t as i32;

    t = f32::from(max) - f32::from(min);
    t /= 10.0f32;
    t += 30.02784f32;
    t /= 15.01392f32;
    t += 0.5f32;
    (ref_bin_offset, t as u8)
}

impl<I: I2c, D: DelayNs> Device<I, D> {
    /// Programs the motion indicator for resolution, with the ULD's default
    /// settings: motion is detected between 400 and 1500 mm. The resolution
    /// must match the one set with set_resolution.
    pub async fn motion_indicator_init(&mut self, resolution: u8) -> Result<(), Error<I::Error>> {
        if resolution != RESOLUTION_4X4 && resolution != RESOLUTION_8X8 {
            return Err(Error::Invalid);
        }
        self.motion.resolution = resolution;
        self.motion.ref_bin_offset = 13633;
        self.motion.feature_length = 9;
        self.write_motion_config().await
    }

    /// Sets the distance window of the motion indicator to min to max mm,
    /// see [`valid_motion_distance`]. It must be called after
    /// motion_indicator_init.
    pub async fn set_motion_distance(&mut self, min: u16, max: u16) -> Result<(), Error<I::Error>> {
        if self.motion.resolution == 0 || !valid_motion_distance(min, max) {
            return Err(Error::Invalid);
        }
        (self.motion.ref_bin_offset, self.motion.feature_length) = motion_fields(min, max);
        self.write_motion_config().await
    }

    /// Writes the motion indicator configuration.
    async fn write_motion_config(&mut self) -> Result<(), Error<I::Error>> {
        // The VL53L7CX_Motion_Configuration structure, in host format, is
        // built in place in the temporary buffer.
        let (resolution, ref_bin_offset, feature_length) = (
            self.motion.resolution,
            self.motion.ref_bin_offset,
            self.motion.feature_length,
        );
        let cfg = &mut self.buf()[4..4 + MOTION_CONFIG_SIZE];
        cfg.fill(0);
        put32(&mut cfg[0..], ref_bin_offset as u32); // ref_bin_offset
        put32(&mut cfg[4..], 2883584); // detection_threshold
        // extra_noise_sigma and null_den_clip_value stay 0.
        cfg[16..24].copy_from_slice(&[
            6,              // mem_update_mode
            2,              // mem_update_choice
            4,              // sum_span
            feature_length, // feature_length
            16,             // nb_of_aggregates
            16,             // nb_of_temporal_accumulations
            1,              // min_nb_for_global_detection
            8,              // global_indicator_format_1
                            // global_indicator_format_2 and the spares stay 0.
        ]);

        let map_id = &mut cfg[28..28 + 64];
        for (i, id) in map_id.iter_mut().enumerate() {
            *id = if resolution == RESOLUTION_4X4 {
                if i < usize::from(RESOLUTION_4X4) {
                    i as u8
                } else {
                    0xff
                } // -1
            } else {
                ((i % 8) / 2 + 4 * (i / 16)) as u8
            };
        }
        // indicator_format_1 and indicator_format_2 stay 0.
        self.dci_write_buf(DCI_MOTION_CONFIG, MOTION_CONFIG_SIZE).await
    }

    /// Programs a detection threshold on the motion indicator of each of the
    /// 16 zones of resolution 4x4: once detection thresholds are enabled,
    /// the sensor pulls its INT pin low for a frame in which the motion of
    /// any zone exceeds threshold, 1 to [`MAX_MOTION_THRESHOLD`].
    pub async fn set_motion_thresholds(&mut self, threshold: u16) -> Result<(), Error<I::Error>> {
        if !(1..=MAX_MOTION_THRESHOLD).contains(&threshold) {
            return Err(Error::Invalid);
        }

        // Set the valid target list
        self.dci_write(DCI_DET_THRESH_VALID_STATUS, &[0x05; 8]).await?;

        // The array of VL53L7CX_DetectionThresholds, in host format, is
        // built in place in the temporary buffer.
        let t = &mut self.buf()[4..4 + NB_THRESHOLDS * THRESHOLD_SIZE];
        t.fill(0);
        let v = u32::from(threshold) * 65535;
        for zone in 0..usize::from(RESOLUTION_4X4) {
            let e = &mut t[zone * THRESHOLD_SIZE..];
            put32(&mut e[0..], v); // param_low_thresh
            put32(&mut e[4..], v); // param_high_thresh
            e[8] = MEASURE_MOTION;
            e[9] = GREATER_THAN_MAX;
            e[10] = zone as u8;
            if zone == usize::from(RESOLUTION_4X4) - 1 {
                e[10] |= LAST_THRESHOLD;
            }
            e[11] = 0; // no mathematic operation
        }
        self.dci_write_buf(DCI_DET_THRESH_START, NB_THRESHOLDS * THRESHOLD_SIZE)
            .await
    }

    /// Enables or disables the interrupt on the programmed thresholds. When
    /// disabled, INT pulses on every frame.
    pub async fn enable_detection_thresholds(&mut self, enable: bool) -> Result<(), Error<I::Error>> {
        let mut global = [0x01, 0x00, 0x01, 0x00];
        let mut cfg = 0x0C;
        if enable {
            global[1] = 0x01;
            cfg = 0x04;
        }

        // Set the global interrupt config
        self.dci_replace(DCI_DET_THRESH_GLOBAL_CONFIG, 8, &global, 0x00)
            .await?;
        // Update the interrupt config
        self.dci_replace(DCI_DET_THRESH_CONFIG, 20, &[cfg], 0x11).await
    }
}
