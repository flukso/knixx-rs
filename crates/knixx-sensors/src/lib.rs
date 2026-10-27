//! Drivers for the SHT2x and VCNL4040 sensors
#![no_std]

#[cfg(test)]
extern crate std;

pub mod sht2x;
pub mod vcnl4040;

#[cfg(test)]
mod testutil {
    use core::future::Future;
    use core::pin::pin;
    use core::task::{Context, Poll, Waker};

    /// block_on polls fut to completion with a no-op waker.
    pub fn block_on<F: Future>(fut: F) -> F::Output {
        let mut fut = pin!(fut);
        let mut cx = Context::from_waker(Waker::noop());
        loop {
            if let Poll::Ready(v) = fut.as_mut().poll(&mut cx) {
                return v;
            }
        }
    }
}
