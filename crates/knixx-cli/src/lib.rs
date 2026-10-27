//! The console commands shared by the firmware and the host shell
//!
//! The commands work through a [`Ctx`], which the firmware and the host
//! shell implement; a caller adds its own commands to the table that
//! [`commands`] starts, and runs lines with [`exec`].
#![no_std]

#[cfg(test)]
extern crate std;

#[cfg(test)]
mod tests;

use core::fmt::{self, Write};
use core::sync::atomic::Ordering;

use knixx_bus::{Command, Flags};
use knixx_device::config;
use knixx_device::thunder::Device;

const SWITCH_USAGE: &str = "usage: switch <A|B> <on|off>";
const SNIFF_USAGE: &str = "usage: sniff <on|off>";
const STAT_USAGE: &str = "usage: stat";
const CONFIG_USAGE: &str = "usage: config [dump | init | get <key> | set <key> <value>[,<value>...]]";
const QUEUE_FULL: &str = "tx queue full";

/// The most fields exec splits a line into: every line of the firmware's
/// line editor fits. On a longer line the last field holds the rest of it.
pub const MAX_FIELDS: usize = 128;

/// The longest value of config set once its fields are joined: every line of
/// the firmware's line editor fits.
const MAX_JOINED: usize = 256;

/// The number of commands that [`commands`] returns.
pub const NUM_SHARED: usize = 4;

/// What the commands work through: the device configuration, the command
/// queue of the bus engine, the flags it shares and the configuration store.
pub trait Ctx {
    /// The error of save, shown as "(not saved: <err>)".
    type SaveError: fmt::Display;

    /// The current device configuration.
    fn device(&self) -> Device;
    /// Replaces the device configuration.
    fn set_device(&mut self, dev: Device);
    /// Queues cmd for the bus engine without blocking. It reports false when
    /// the queue is full.
    fn send(&mut self, cmd: Command) -> bool;
    /// The flags shared with the bus engine.
    fn flags(&self) -> &Flags;
    /// Persists a configuration file.
    fn save(&mut self, data: &[u8]) -> Result<(), Self::SaveError>;
}

/// What the caller does after a command.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Action {
    None,
    /// Ends a host shell session.
    Exit,
    /// Reboots the board, into the mode of the given code.
    Reboot(u8),
    /// Turns the BLE Nordic UART Service on or off.
    Nus(bool),
}

/// Cmd is a console command. Run writes its replies to the writer.
pub struct Cmd<C> {
    pub name: &'static str,
    pub help: &'static str,
    pub run: fn(&mut C, &[&str], &mut dyn Write) -> Action,
}

impl<C> Clone for Cmd<C> {
    fn clone(&self) -> Self {
        *self
    }
}

impl<C> Copy for Cmd<C> {}

/// commands returns the shared console commands. Commands that talk to the
/// transceiver are queued with Ctx::send; configuration changes are applied
/// with Ctx::set_device and saved with Ctx::save.
pub const fn commands<C: Ctx>() -> [Cmd<C>; NUM_SHARED] {
    [
        Cmd {
            name: "switch",
            help: SWITCH_USAGE,
            run: switch_cmd::<C>,
        },
        Cmd {
            name: "sniff",
            help: SNIFF_USAGE,
            run: sniff_cmd::<C>,
        },
        Cmd {
            name: "stat",
            help: "report the transceiver state and system status",
            run: stat_cmd::<C>,
        },
        Cmd {
            name: "config",
            help: CONFIG_USAGE,
            run: config_cmd::<C>,
        },
    ]
}

fn switch_cmd<C: Ctx>(ctx: &mut C, args: &[&str], w: &mut dyn Write) -> Action {
    match parse_switch(args) {
        Some((name, on)) => switch(ctx, name, on, w),
        None => line(w, SWITCH_USAGE),
    }
    Action::None
}

fn sniff_cmd<C: Ctx>(ctx: &mut C, args: &[&str], w: &mut dyn Write) -> Action {
    match args {
        [] => {
            if ctx.flags().sniff.load(Ordering::Relaxed) {
                line(w, "sniff is on");
            } else {
                line(w, "sniff is off");
            }
        }
        [arg] => match parse_on_off(arg) {
            Some(on) => ctx.flags().sniff.store(on, Ordering::Relaxed),
            None => line(w, SNIFF_USAGE),
        },
        _ => line(w, SNIFF_USAGE),
    }
    Action::None
}

fn stat_cmd<C: Ctx>(ctx: &mut C, args: &[&str], w: &mut dyn Write) -> Action {
    if !args.is_empty() {
        line(w, STAT_USAGE);
        return Action::None;
    }
    enqueue(ctx, Command::Stat, w);
    Action::None
}

fn config_cmd<C: Ctx>(ctx: &mut C, args: &[&str], w: &mut dyn Write) -> Action {
    match args {
        [] => list(&ctx.device(), w),
        ["dump"] => {
            // One set command per key, so that replaying them on another
            // device reproduces this configuration and its ini file exactly.
            let dev = ctx.device();
            for key in config::KEYS {
                if let Ok(v) = config::get(&dev, key) {
                    let _ = writeln!(w, "config set {key} {v}");
                }
            }
        }
        ["init"] => config_init(ctx, w),
        ["get", key] => match config::get(&ctx.device(), key) {
            Ok(v) => line(w, &v),
            Err(err) => {
                let _ = writeln!(w, "{err}");
            }
        },
        ["set", key, values @ ..] if !values.is_empty() => config_set(ctx, key, values, w),
        _ => line(w, CONFIG_USAGE),
    }
    Action::None
}

fn config_init<C: Ctx>(ctx: &mut C, w: &mut dyn Write) {
    let def = match config::parse(config::DEFAULT) {
        Ok(def) => def,
        Err(err) => {
            let _ = writeln!(w, "{err}");
            return;
        }
    };
    if !ctx.send(Command::Config(def)) {
        line(w, QUEUE_FULL);
        return;
    }
    ctx.set_device(def);
    // The embedded file is saved verbatim, so that the stored knixx.ini is
    // identical to the one in the firmware.
    match ctx.save(config::DEFAULT) {
        Ok(()) => line(w, "knixx.ini reset to the built-in default (saved)"),
        Err(err) => {
            let _ = writeln!(w, "knixx.ini reset to the built-in default (not saved: {err})");
        }
    }
    list(&def, w);
}

fn config_set<C: Ctx>(ctx: &mut C, key: &str, values: &[&str], w: &mut dyn Write) {
    // A group list may be typed with spaces after the commas.
    let mut buf = [0u8; MAX_JOINED];
    let Some(value) = join(values, &mut buf) else {
        line(w, CONFIG_USAGE);
        return;
    };
    let mut dev = ctx.device();
    if let Err(err) = config::set(&mut dev, key, value) {
        let _ = writeln!(w, "{err}");
        return;
    }
    // The device is only replaced once the engine takes it, which leaves it
    // untouched when the queue is full.
    if !ctx.send(Command::Config(dev)) {
        line(w, QUEUE_FULL);
        return;
    }
    ctx.set_device(dev);
    let v = config::get(&dev, key).ok();
    let v = v.as_deref().unwrap_or("");
    let mut file = [0u8; config::MAX_FILE];
    match ctx.save(config::format(&dev, &mut file)) {
        Ok(()) => {
            let _ = writeln!(w, "{key} = {v} (saved)");
        }
        Err(err) => {
            let _ = writeln!(w, "{key} = {v} (not saved: {err})");
        }
    }
}

/// join concatenates fields into buf. It returns None when they do not fit.
fn join<'b>(fields: &[&str], buf: &'b mut [u8; MAX_JOINED]) -> Option<&'b str> {
    let mut n = 0;
    for f in fields {
        let end = n + f.len();
        buf.get_mut(n..end)?.copy_from_slice(f.as_bytes());
        n = end;
    }
    // Whole strs fill buf.
    core::str::from_utf8(&buf[..n]).ok()
}

/// list prints every configuration key with its value.
fn list(dev: &Device, w: &mut dyn Write) {
    for key in config::KEYS {
        if let Ok(v) = config::get(dev, key) {
            let _ = writeln!(w, "{key} = {v}");
        }
    }
}

/// exec runs the command on line, with the built-in help and exit. It
/// returns what the caller does next: Exit when the line asked to exit.
pub fn exec<C>(cmds: &[Cmd<C>], ctx: &mut C, line: &str, w: &mut dyn Write) -> Action {
    let mut fields = [""; MAX_FIELDS];
    let n = split_fields(line, &mut fields);
    let Some((&name, args)) = fields[..n].split_first() else {
        return Action::None;
    };
    match name {
        "exit" => return Action::Exit,
        "help" => {
            for c in cmds {
                let _ = writeln!(w, "  {:<8} {}", c.name, c.help);
            }
            let _ = writeln!(w, "  {:<8} list the commands", "help");
            return Action::None;
        }
        _ => {}
    }
    if let Some(c) = cmds.iter().find(|c| c.name == name) {
        return (c.run)(ctx, args, w);
    }
    let _ = w.write_str("unknown command ");
    let _ = write_quoted(w, name);
    let _ = w.write_str(", try help\n");
    Action::None
}

/// split_fields splits line around runs of white space into fields, as Go's
/// strings.Fields, and returns their number. When line has more fields than
/// fit, the last one holds the rest of line.
fn split_fields<'a>(line: &'a str, fields: &mut [&'a str; MAX_FIELDS]) -> usize {
    let mut rest = line.trim_start();
    let mut n = 0;
    while !rest.is_empty() {
        if n == MAX_FIELDS - 1 {
            fields[n] = rest.trim_end();
            return MAX_FIELDS;
        }
        let end = rest.find(char::is_whitespace).unwrap_or(rest.len());
        fields[n] = &rest[..end];
        n += 1;
        rest = rest[end..].trim_start();
    }
    n
}

/// switch queues the command that sets the switch with the case-insensitive
/// name to on. It is what the switch command does, and what the wall
/// switches do; errors are written to w.
pub fn switch<C: Ctx>(ctx: &mut C, name: &str, on: bool, w: &mut dyn Write) {
    let Some(switch) = ctx.device().switch_index(name) else {
        let _ = w.write_str("unknown switch: ");
        let _ = w.write_str(name);
        let _ = w.write_str("\n");
        return;
    };
    enqueue(ctx, Command::Switch { switch, on }, w);
}

/// enqueue hands cmd to the bus engine without blocking the console.
fn enqueue<C: Ctx>(ctx: &mut C, cmd: Command, w: &mut dyn Write) {
    if !ctx.send(cmd) {
        line(w, QUEUE_FULL);
    }
}

/// line writes s and a newline.
fn line(w: &mut dyn Write, s: &str) {
    let _ = w.write_str(s);
    let _ = w.write_str("\n");
}

/// parse_switch splits the arguments of a switch command into the switch
/// name and the requested state.
fn parse_switch<'a>(args: &[&'a str]) -> Option<(&'a str, bool)> {
    match args {
        [name, state] => Some((name, parse_on_off(state)?)),
        _ => None,
    }
}

fn parse_on_off(arg: &str) -> Option<bool> {
    if arg.eq_ignore_ascii_case("on") {
        Some(true)
    } else if arg.eq_ignore_ascii_case("off") {
        Some(false)
    } else {
        None
    }
}

/// write_quoted writes s as Go's %q does.
fn write_quoted(w: &mut dyn Write, s: &str) -> fmt::Result {
    w.write_char('"')?;
    for c in s.chars() {
        match c {
            '"' => w.write_str("\\\"")?,
            '\\' => w.write_str("\\\\")?,
            '\x07' => w.write_str("\\a")?,
            '\x08' => w.write_str("\\b")?,
            '\x0c' => w.write_str("\\f")?,
            '\n' => w.write_str("\\n")?,
            '\r' => w.write_str("\\r")?,
            '\t' => w.write_str("\\t")?,
            '\x0b' => w.write_str("\\v")?,
            ' '..='~' => w.write_char(c)?,
            c if c < ' ' || c == '\x7f' => write!(w, "\\x{:02x}", c as u32)?,
            c if is_print(c) => w.write_char(c)?,
            c if (c as u32) < 0x10000 => write!(w, "\\u{:04x}", c as u32)?,
            c => write!(w, "\\U{:08x}", c as u32)?,
        }
    }
    w.write_char('"')
}

/// is_print approximates Go's strconv.IsPrint with Rust's escape_debug.
fn is_print(c: char) -> bool {
    let mut e = c.escape_debug();
    e.next() == Some(c) && e.next().is_none()
}
