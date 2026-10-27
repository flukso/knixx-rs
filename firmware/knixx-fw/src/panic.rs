//! A panic resets the chip. A record of it for the next boot follows with
//! the console.
use core::panic::PanicInfo;

#[panic_handler]
fn panic(_info: &PanicInfo) -> ! {
    cortex_m::peripheral::SCB::sys_reset()
}
