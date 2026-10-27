//! The climate and light sensors: the SHT20 (temperature, humidity) and the
//! ambient light sensor of the VCNL4040, both on I2C1. They are read 5 s
//! after boot and then every minute, on an absolute ticker so that the
//! readings stay a minute apart, and the readings are queued for the bus.
//! A sensor that cannot be configured or read is reported and retried at
//! the next tick.

use core::cell::Cell;
use core::fmt::Write;

use embassy_sync::blocking_mutex::Mutex;
use embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex;
use embassy_time::{Delay, Duration, Instant, Ticker, Timer};
use knixx_bus::Command;
use knixx_cli::Action;
use knixx_sensors::{sht2x, vcnl4040};
use knixx_ui::led::Mode;

use crate::console::{self, Ctx, Out};
use crate::{i2c, leds};

const INTERVAL: Duration = Duration::from_secs(60);
/// The delay of the first reading after boot.
const FIRST: Duration = Duration::from_secs(5);

/// The latest readings, for the sensor command, with the uptime at which
/// each measurement was triggered.
#[derive(Clone, Copy, Default)]
struct Reading {
    climate: Option<(Instant, i32, i32)>, // m°C, c%
    light: Option<(Instant, u32)>,        // dlx
}

static READING: Mutex<CriticalSectionRawMutex, Cell<Reading>> = Mutex::new(Cell::new(Reading {
    climate: None,
    light: None,
}));

/// Reports a failure on the console and blinks red.
fn fail(args: core::fmt::Arguments) {
    let _ = writeln!(Out, "{args}");
    leds::signal(Mode::Error);
}

/// Queues cmd for the bus, reporting a full queue.
fn send(cmd: Command) {
    if console::CMDS.try_send(cmd).is_err() {
        fail(format_args!("tx queue full"));
    }
}

#[embassy_executor::task]
pub async fn task(bus: &'static i2c::Bus) -> ! {
    let mut sensor = sht2x::Device::new(i2c::Device::new(bus), Delay);
    let mut light = vcnl4040::Device::new(i2c::Device::new(bus), Delay);
    let mut sensor_configured = false;
    let mut light_configured = false;

    Timer::after(FIRST).await;
    let mut ticker = Ticker::every(INTERVAL);
    loop {
        if !sensor_configured {
            match sensor.configure().await {
                Ok(()) => sensor_configured = true,
                Err(e) => fail(format_args!("sht20: {e}")),
            }
        }
        if sensor_configured {
            let at = Instant::now();
            match sensor.read_temperature().await {
                Err(e) => fail(format_args!("sht20: temperature: {e}")),
                Ok(t) => match sensor.read_humidity().await {
                    Err(e) => fail(format_args!("sht20: humidity: {e}")),
                    Ok(h) => {
                        READING.lock(|r| {
                            r.set(Reading {
                                climate: Some((at, t, h)),
                                ..r.get()
                            })
                        });
                        send(Command::Sensor {
                            temperature: t,
                            humidity: h,
                        });
                    }
                },
            }
        }

        if !light_configured {
            match light.configure().await {
                Ok(()) => light_configured = true,
                Err(e) => fail(format_args!("vcnl4040: {e}")),
            }
        }
        if light_configured {
            let at = Instant::now();
            match light.read_illuminance().await {
                Err(e) => {
                    light_configured = false;
                    fail(format_args!("vcnl4040: {e}"));
                }
                Ok(lux10) => {
                    READING.lock(|r| {
                        r.set(Reading {
                            light: Some((at, lux10)),
                            ..r.get()
                        })
                    });
                    send(Command::Light {
                        illuminance: lux10 as i32,
                    });
                }
            }
        }
        ticker.next().await;
    }
}

/// The sensor command: the latest readings, as the Go firmware shows them.
pub fn command(_: &mut Ctx, args: &[&str], w: &mut dyn Write) -> Action {
    if !args.is_empty() {
        let _ = writeln!(w, "usage: sensor");
        return Action::None;
    }
    let now = Instant::now();
    let r = READING.lock(|r| r.get());
    match r.climate {
        None => {
            let _ = writeln!(w, "no temperature and humidity reading yet");
        }
        Some((at, t, h)) => {
            // Go printed the float32 quotients with %.2f.
            let _ = writeln!(
                w,
                "temperature {:.2} C, humidity {:.2} %RH at {} ms ({} s ago)",
                t as f32 / 1000.0,
                h as f32 / 100.0,
                at.as_millis(),
                now.saturating_duration_since(at).as_secs()
            );
        }
    }
    match r.light {
        None => {
            let _ = writeln!(w, "no illuminance reading yet");
        }
        Some((at, lux10)) => {
            let saturated = if lux10 >= vcnl4040::MAX_LUX10 {
                " (saturated)"
            } else {
                ""
            };
            let _ = writeln!(
                w,
                "illuminance {}.{} lx{saturated} at {} ms ({} s ago)",
                lux10 / 10,
                lux10 % 10,
                at.as_millis(),
                now.saturating_duration_since(at).as_secs()
            );
        }
    }
    Action::None
}
