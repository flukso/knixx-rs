//! The VL53L7CX time-of-flight sensor on I2C1, for occupancy. It runs in
//! autonomous ranging mode at 4x4 with the ranging frequency, integration
//! time and distance window of the configuration's occupancy settings, and
//! detection thresholds on its motion indicator: the sensor pulses INT for
//! each frame in which a zone's motion exceeds the threshold, and each
//! pulse queues a motion command for the bus and blinks green.
//!
//! A change of the settings by the console is picked up by polling the
//! configuration: ranging stops, the changed settings are programmed and
//! ranging starts again. A sensor that fails is tried again after a minute.

use core::cell::RefCell;
use core::fmt::{self, Write};
use core::sync::atomic::{AtomicU32, Ordering};

use embassy_nrf::Peri;
use embassy_nrf::gpio::{Input, Level, Output, OutputDrive, Pull};
use embassy_nrf::peripherals::{P0_13, P0_14, P0_15};
use embassy_sync::blocking_mutex::Mutex;
use embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex;
use embassy_time::{Delay, Duration, Instant, Timer};
use knixx_bus::Command;
use knixx_cli::Action;
use knixx_device::thunder::{self, Device as Config, Occupancy};
use knixx_ui::led::Mode;
use knixx_vl53l7cx::{Device, Motion, RANGING_AUTONOMOUS, RESOLUTION_4X4};
use static_cell::StaticCell;

use crate::console::{self, Ctx, Out};
use crate::{i2c, leds};

/// The period at which motion pulses are picked up and setting changes
/// applied.
const POLL: Duration = Duration::from_millis(50);
/// The delay of a new attempt after the sensor failed.
const RETRY: Duration = Duration::from_secs(60);

type Sensor = Device<i2c::Device, Delay>;

/// The pins of the sensor, on the Feather's SPI header.
pub struct Pins {
    /// SCK: INT, open drain with a 47 kΩ pull-up.
    pub int: Peri<'static, P0_14>,
    /// MOSI: LPn, high enables I2C.
    pub lpn: Peri<'static, P0_13>,
    /// MISO: I2C_RST, active high.
    pub rst: Peri<'static, P0_15>,
}

/// The INT pulses; the edge task only counts them.
static PULSES: AtomicU32 = AtomicU32::new(0);

/// The ranging settings of a configuration that the sensor is programmed
/// with.
#[derive(Clone, Copy, PartialEq, Eq, Default)]
struct Settings {
    threshold: u16,
    frequency: u16,
    integration: u16,
    min_distance: u16,
    max_distance: u16,
}

/// Returns the settings of dev. An invalid distance window is replaced by
/// the one of last, or by the default one when last has none.
fn settings_of(dev: &Config, last: Settings) -> Settings {
    let o = &dev.occupancy;
    let mut s = Settings {
        threshold: o.threshold,
        frequency: o.frequency,
        integration: o.integration,
        min_distance: o.min_distance,
        max_distance: o.max_distance,
    };
    if !o.distance_valid() {
        s.min_distance = last.min_distance;
        s.max_distance = last.max_distance;
        if s.min_distance == 0 {
            s.min_distance = thunder::DEFAULT_MIN_DISTANCE;
            s.max_distance = thunder::DEFAULT_MAX_DISTANCE;
        }
    }
    s
}

/// An error, kept as its text for the tof command.
#[derive(Clone, Copy)]
struct ErrText {
    buf: [u8; 96],
    len: usize,
}

impl ErrText {
    fn of(e: &dyn fmt::Display) -> ErrText {
        let mut t = ErrText { buf: [0; 96], len: 0 };
        let _ = write!(t, "{e}");
        t
    }

    fn as_str(&self) -> &str {
        core::str::from_utf8(&self.buf[..self.len]).unwrap_or("")
    }
}

impl Write for ErrText {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        let n = s.len().min(self.buf.len() - self.len);
        self.buf[self.len..self.len + n].copy_from_slice(&s.as_bytes()[..n]);
        self.len += n;
        Ok(())
    }
}

/// What the tof command reports. Only the ToF task writes it.
#[derive(Clone, Copy)]
struct Status {
    err: Option<ErrText>,
    ranging: bool,
    boot_ms: u64,
    /// The ranging settings the sensor runs with.
    applied: Settings,
    motions: u32,
    /// The time of the last motion, and the per zone intensities of its
    /// frame.
    last_motion: Instant,
    motion: Motion,
}

static STATUS: Mutex<CriticalSectionRawMutex, RefCell<Status>> = Mutex::new(RefCell::new(Status {
    err: None,
    ranging: false,
    boot_ms: 0,
    applied: Settings {
        threshold: 0,
        frequency: 0,
        integration: 0,
        min_distance: 0,
        max_distance: 0,
    },
    motions: 0,
    last_motion: Instant::MIN,
    motion: Motion {
        global1: 0,
        global2: 0,
        status: 0,
        nb_detected: 0,
        nb_aggregates: 0,
        values: [0; 32],
    },
}));

fn status(f: impl FnOnce(&mut Status)) {
    STATUS.lock(|s| f(&mut s.borrow_mut()));
}

/// Reports a failure on the console and blinks red. The driver's errors
/// start with "vl53l7cx: " already (the Go firmware printed it twice).
fn fail(e: &dyn fmt::Display) {
    let _ = writeln!(Out, "{e}");
    leds::signal(Mode::Error);
}

/// Counts the falling edges of INT.
#[embassy_executor::task]
async fn edges(mut int: Input<'static>) -> ! {
    loop {
        int.wait_for_falling_edge().await;
        PULSES.fetch_add(1, Ordering::Relaxed);
    }
}

#[embassy_executor::task]
pub async fn task(spawner: embassy_executor::Spawner, bus: &'static i2c::Bus, pins: Pins) -> ! {
    let _lpn = Output::new(pins.lpn, Level::High, OutputDrive::Standard);
    let mut rst = Output::new(pins.rst, Level::Low, OutputDrive::Standard);
    spawner.spawn(edges(Input::new(pins.int, Pull::None)).unwrap());

    static SENSOR: StaticCell<Sensor> = StaticCell::new();
    let tof = SENSOR.init(Device::new(i2c::Device::new(bus), Delay));

    let mut first = true;
    loop {
        let applied = STATUS.lock(|s| s.borrow().applied);
        let s = settings_of(&console::device(), applied);
        let started = start(tof, &mut rst, s).await;
        if first {
            // The board has started once the sensor ranges or failed to.
            first = false;
            leds::cancel(Mode::Init);
        }
        match started {
            Err(e) => {
                status(|st| {
                    st.err = Some(ErrText::of(&e));
                    st.ranging = false;
                });
                fail(&e);
            }
            Ok(boot) => {
                status(|st| {
                    st.err = None;
                    st.ranging = true;
                    st.applied = s;
                    st.boot_ms = boot.as_millis();
                });
                let e = watch(tof).await;
                status(|st| {
                    st.err = Some(ErrText::of(&e));
                    st.ranging = false;
                });
                fail(&e);
            }
        }
        Timer::after(RETRY).await;
    }
}

type Error = knixx_vl53l7cx::Error<<i2c::Device as embedded_hal_async::i2c::ErrorType>::Error>;

/// Resets the sensor's I2C interface, boots and configures the sensor and
/// starts ranging. It returns how long the boot took.
async fn start(tof: &mut Sensor, rst: &mut Output<'static>, s: Settings) -> Result<Duration, Error> {
    rst.set_high();
    Timer::after_millis(1).await;
    rst.set_low();
    Timer::after_millis(1).await;

    let begin = Instant::now();
    if !tof.is_alive().await? {
        return Err(knixx_vl53l7cx::Error::NotAlive);
    }
    tof.init().await?;
    let boot = begin.elapsed();
    tof.set_resolution(RESOLUTION_4X4).await?;
    tof.set_ranging_frequency(s.frequency as u8).await?;
    tof.set_ranging_mode(RANGING_AUTONOMOUS).await?;
    tof.set_integration_time(s.integration as u32).await?;
    tof.motion_indicator_init(RESOLUTION_4X4).await?;
    tof.set_motion_distance(s.min_distance, s.max_distance).await?;
    tof.set_motion_thresholds(s.threshold).await?;
    tof.enable_detection_thresholds(true).await?;
    PULSES.store(0, Ordering::Relaxed);
    tof.start_ranging().await?;
    Ok(boot)
}

/// Turns INT pulses into motion commands and applies setting changes,
/// until the sensor fails.
async fn watch(tof: &mut Sensor) -> Error {
    let mut seen = 0;
    loop {
        Timer::after(POLL).await;

        let n = PULSES.load(Ordering::Relaxed);
        if n != seen {
            let now = Instant::now();
            status(|st| {
                st.motions += n.wrapping_sub(seen);
                st.last_motion = now;
            });
            seen = n;
            leds::signal(Mode::Occupancy);
            if console::CMDS.try_send(Command::Motion).is_err() {
                let _ = writeln!(Out, "tx queue full");
                leds::signal(Mode::Error);
            }
            // The frame that raised INT, for the tof command.
            if let Ok(m) = tof.motion().await {
                status(|st| st.motion = m);
            }
        }

        // Settings cannot change while ranging.
        let old = STATUS.lock(|s| s.borrow().applied);
        let s = settings_of(&console::device(), old);
        if s != old {
            if let Err(e) = apply(tof, old, s).await {
                return e;
            }
            status(|st| st.applied = s);
        }
    }
}

/// Stops ranging, programs the settings of s that differ from old and
/// starts ranging again.
async fn apply(tof: &mut Sensor, old: Settings, s: Settings) -> Result<(), Error> {
    tof.stop_ranging().await?;
    if s.frequency != old.frequency {
        tof.set_ranging_frequency(s.frequency as u8).await?;
    }
    if s.integration != old.integration {
        tof.set_integration_time(s.integration as u32).await?;
    }
    if s.min_distance != old.min_distance || s.max_distance != old.max_distance {
        tof.set_motion_distance(s.min_distance, s.max_distance).await?;
    }
    if s.threshold != old.threshold {
        tof.set_motion_thresholds(s.threshold).await?;
    }
    tof.start_ranging().await
}

/// The tof command: the state of the sensor and its latest motion, as the
/// Go firmware shows them.
pub fn command(_: &mut Ctx, args: &[&str], w: &mut dyn Write) -> Action {
    if !args.is_empty() {
        let _ = writeln!(w, "usage: tof");
        return Action::None;
    }
    let s = STATUS.lock(|s| *s.borrow());
    if let Some(e) = &s.err {
        let _ = writeln!(w, "error: {}", e.as_str());
        return Action::None;
    }
    if !s.ranging {
        let _ = writeln!(w, "starting");
        return Action::None;
    }
    let occupied = if console::FLAGS.occupied.load(Ordering::Relaxed) {
        "yes"
    } else {
        "no"
    };
    let a = s.applied;
    let _ = writeln!(
        w,
        "ranging 4x4 at {} Hz, integration {} ms, threshold {}, booted in {} ms",
        a.frequency, a.integration, a.threshold, s.boot_ms
    );
    let o = Occupancy {
        frequency: a.frequency,
        integration: a.integration,
        ..Occupancy::default()
    };
    if o.period_stretched() {
        let _ = writeln!(
            w,
            "warning: integration time + 1 ms exceeds the ranging period; the sensor lowers the frame rate"
        );
    }
    let _ = writeln!(w, "motion window {}-{} mm", a.min_distance, a.max_distance);
    if !console::device().occupancy.distance_valid() {
        let _ = writeln!(
            w,
            "warning: min_distance/max_distance invalid (need min < max, span <= {} mm); keeping {}-{} mm",
            thunder::MAX_MOTION_SPAN,
            a.min_distance,
            a.max_distance
        );
    }
    let _ = writeln!(w, "occupied {occupied}, {} motion frames", s.motions);
    if s.motions == 0 {
        return Action::None;
    }
    let _ = writeln!(
        w,
        "last motion {} s ago, per zone:",
        s.last_motion.elapsed().as_secs()
    );
    for row in 0..4 {
        for col in 0..4 {
            let _ = write!(w, " {:6}", s.motion.values[4 * row + col]);
        }
        let _ = writeln!(w);
    }
    Action::None
}
