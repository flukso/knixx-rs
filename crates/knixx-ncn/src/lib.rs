//! The host protocol of the onsemi NCN5121 KNX transceiver
#![no_std]

#[cfg(test)]
extern crate std;

mod service;

pub use service::{
    Describe, FrameState, Parser, Service, State, SystemStat, con_positive, decode_frame_state, decode_state,
    decode_system_stat, describe,
};

// Services from the host controller to the NCN5121.
pub const U_RESET_REQ: u8 = 0x01;
pub const U_STATE_REQ: u8 = 0x02;
pub const U_SET_BUSY_REQ: u8 = 0x03;
pub const U_QUIT_BUSY_REQ: u8 = 0x04;
pub const U_BUSMON_REQ: u8 = 0x05;
pub const U_SYSTEM_STAT_REQ: u8 = 0x0d;
pub const U_L_DATA_START_REQ: u8 = 0x80;
pub const U_L_DATA_END_REQ: u8 = 0x40;

/// The longest frame the 6 index bits of U_L_DataCont.req can address
/// without U_L_DataOffset.req.
pub const MAX_FRAME_LEN: usize = 64;

/// An error of the host protocol.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Error {
    /// The frame is shorter than 2 or longer than [`MAX_FRAME_LEN`] octets.
    FrameLen,
}

impl core::fmt::Display for Error {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Error::FrameLen => f.write_str("ncn: invalid frame length"),
        }
    }
}

impl core::error::Error for Error {}

/// TxBuf holds the request tx_data builds.
pub type TxBuf = [u8; 2 * MAX_FRAME_LEN];

/// tx_data builds the request that sends frame in req and returns it. The
/// caller writes it as a single request, so that no gaps open up between
/// the control and data bytes.
pub fn tx_data<'a>(frame: &[u8], req: &'a mut TxBuf) -> Result<&'a [u8], Error> {
    if frame.len() < 2 || frame.len() > MAX_FRAME_LEN {
        return Err(Error::FrameLen);
    }
    let last = frame.len() - 1;
    for (i, &b) in frame.iter().enumerate() {
        let offset = if i == last {
            U_L_DATA_END_REQ
        } else {
            U_L_DATA_START_REQ
        };
        req[2 * i] = offset + i as u8;
        req[2 * i + 1] = b;
    }
    Ok(&req[..2 * frame.len()])
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::string::ToString;
    use std::vec::Vec;

    #[test]
    fn tx_data_requests() {
        let frame = [0xbc, 0x78, 0x01, 0x00, 0x02, 0x81, 0x00, 0x80, 0x39];
        let mut req = [0; 2 * MAX_FRAME_LEN];
        let got = tx_data(&frame, &mut req).unwrap();
        let want = [
            0x80, 0xbc, 0x81, 0x78, 0x82, 0x01, 0x83, 0x00, 0x84, 0x02, 0x85, 0x81, 0x86, 0x00, 0x87, 0x80,
            0x48, 0x39,
        ];
        assert_eq!(got, want);
        assert_eq!(tx_data(&[0; MAX_FRAME_LEN + 1], &mut req), Err(Error::FrameLen));
        assert_eq!(tx_data(&[0], &mut req), Err(Error::FrameLen));
        assert_eq!(Error::FrameLen.to_string(), "ncn: invalid frame length");
    }

    type Event = (Service, u8);

    fn feed(data: &[u8]) -> Vec<Event> {
        let mut p = Parser::new();
        data.iter().filter_map(|&b| p.feed(b)).collect()
    }

    #[test]
    fn parser() {
        use Service::*;
        let tests: &[(&str, &[u8], &[Event])] = &[
            (
                "clean echo",
                &[0xbc, 0x78, 0x01, 0x00, 0x02, 0x81, 0x00, 0x80, 0x39, 0x13, 0x8b],
                &[(LDataInd, 0), (UFrameStateInd, 0x13), (LDataCon, 0x8b)],
            ),
            (
                "data octets that look like services",
                &[0xbc, 0x07, 0x03, 0x4b, 0x13, 0x81, 0x00, 0x8b, 0x17, 0x0b],
                &[(LDataInd, 0), (LDataCon, 0x0b)],
            ),
            (
                "protocol and receive errors, reset",
                &[0x17, 0x47, 0x03],
                &[(UStateInd, 0x17), (UStateInd, 0x47), (UResetInd, 0x03)],
            ),
            (
                "stat reply",
                &[0x07, 0x4b, 0xfb],
                &[(UStateInd, 0x07), (USystemStatInd, 0xfb)],
            ),
        ];
        for (name, data, want) in tests {
            assert_eq!(feed(data), *want, "{name}");
        }
    }

    #[test]
    fn parser_frame() {
        let short: &[u8] = &[0xbc, 0x78, 0x01, 0x00, 0x02, 0x81, 0x00, 0x80, 0x39];
        let long: &[u8] = &[
            0xbc, 0x11, 0x05, 0x00, 0x03, 0xef, 0x00, 0x80, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14,
            0x00,
        ];
        let mut p = Parser::new();
        assert!(p.frame().is_empty(), "Frame before any frame = {:x?}", p.frame());
        for frame in [short, long, short] {
            for (i, &b) in frame.iter().enumerate() {
                let s = p.feed(b).map(|(s, _)| s);
                let last = i == frame.len() - 1;
                assert_eq!(
                    last,
                    s == Some(Service::LDataInd),
                    "octet {i} of {frame:x?}: service {s:?}"
                );
            }
            assert_eq!(p.frame(), frame);
            // A service after the frame leaves it in place.
            assert_eq!(p.feed(0x8b), Some((Service::LDataCon, 0x8b)));
            assert_eq!(p.frame(), frame);
        }
    }

    #[test]
    fn describe_services() {
        use Service::*;
        let tests: &[(Service, u8, &str)] = &[
            (LDataInd, 0, "L_Data.ind"),
            (LDataCon, 0x8b, "L_Data.con: positive"),
            (LDataCon, 0x0b, "L_Data.con: negative"),
            (UResetInd, 0x03, "U_Reset.ind"),
            (UStateInd, 0x07, "U_State.ind: no errors"),
            (
                UStateInd,
                0x17,
                "U_State.ind: protocol error (invalid command sequence from host)",
            ),
            (
                UStateInd,
                0x47,
                "U_State.ind: receive error (corrupted bytes from host)",
            ),
            (
                UStateInd,
                0xff,
                "U_State.ind: slave collision, receive error (corrupted bytes from host), \
                 transmit error (sent 0, read 1), protocol error (invalid command sequence from host), \
                 thermal warning",
            ),
            (UFrameStateInd, 0x13, "U_FrameState.ind: ok"),
            (
                UFrameStateInd,
                0xd3,
                "U_FrameState.ind: parity or bit error, checksum or length error",
            ),
            (
                USystemStatInd,
                0xfb,
                "U_SystemStat.ind: V20V ok, VDD2 ok, VBUS ok, VFILT ok, XTAL ok, \
                 no thermal warning, mode Normal",
            ),
            (
                USystemStatInd,
                0x5d,
                "U_SystemStat.ind: V20V out of range, VDD2 ok, VBUS out of range, VFILT ok, XTAL ok, \
                 thermal warning, mode Sync",
            ),
        ];
        for &(s, b, want) in tests {
            assert_eq!(describe(s, b).to_string(), want, "describe({s:?}, {b:#04x})");
        }
    }

    #[test]
    fn decoders() {
        assert_eq!(decode_state(0x07).to_string(), "no errors");
        assert_eq!(decode_state(0x0f).to_string(), "thermal warning");
        assert_eq!(decode_frame_state(0x33).to_string(), "timing error");
        assert_eq!(
            decode_system_stat(0x02).to_string(),
            "V20V out of range, VDD2 out of range, VBUS out of range, VFILT out of range, \
             XTAL out of range, no thermal warning, mode Stop"
        );
        assert!(con_positive(0x8b));
        assert!(!con_positive(0x0b));
    }

    #[test]
    fn describe_into_fixed_buffer() {
        use core::fmt::Write;
        struct Buf {
            b: [u8; 256],
            n: usize,
        }
        impl Write for Buf {
            fn write_str(&mut self, s: &str) -> core::fmt::Result {
                let end = self.n + s.len();
                self.b
                    .get_mut(self.n..end)
                    .ok_or(core::fmt::Error)?
                    .copy_from_slice(s.as_bytes());
                self.n = end;
                Ok(())
            }
        }
        let mut w = Buf { b: [0; 256], n: 0 };
        write!(w, "  {}", describe(Service::LDataCon, 0x8b)).unwrap();
        assert_eq!(&w.b[..w.n], b"  L_Data.con: positive");
    }
}
