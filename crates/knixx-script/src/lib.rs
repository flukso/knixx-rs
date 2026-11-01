//! Scripts in Rhai Grain: compiled to bytecode on the host and run on the
//! board, where each one is called on every telegram on the bus and every
//! second, keeps its state, and sends telegrams.
//!
//! A script defines four functions; the last three are methods of its
//! state, an object map that persists from call to call, which they reach
//! as `this`:
//!
//! - `on_info()` returns a map with its `name`, `author`, `date`, and the
//!   `compiled`, `version` and `hash` the compiler fills in;
//! - `on_init()` sets up the state, empty before;
//! - `on_telegram(tg)` gets each telegram on the bus, the board's own too,
//!   as a map;
//! - `on_time(uptime)` is called every second with the uptime in seconds.
//!
//! [`Runtime`] holds the engine, with its limits and the functions scripts
//! can call; [`Script`] is a loaded program with its state. With the
//! `compiler` feature, [`compile`] turns a script into bytecode on the host
//! and checks it.
//!
//! On the board, under `no_std`, it allocates, from the heap that only the
//! scripts use.
#![no_std]

extern crate alloc;
#[cfg(any(test, feature = "compiler"))]
extern crate std;

#[cfg(feature = "compiler")]
mod compile;
#[cfg(test)]
mod tests;

use alloc::boxed::Box;
use alloc::format;
use alloc::rc::Rc;
use alloc::string::{String, ToString};
use alloc::vec;
use alloc::vec::Vec;
use core::cell::RefCell;
use core::fmt;

use knixx_knx::dpt;
use knixx_knx::tp1::{AddrType, Apci, Npci, Priority, Telegram};
use knixx_knx::{GroupAddress, IndividualAddress};
use rhai::grain::{Program, Vm};
use rhai::{Blob, CallFnOptions, Dynamic, Engine, EvalAltResult, FLOAT, INT, Map, Scope};

#[cfg(feature = "compiler")]
pub use compile::{Build, CompileError, Meta, compile, lines};
pub use rhai::grain::{ReadError, Sidecar};

/// The most operations of one call: about 20 ms on the board, which runs
/// some 60 operations per millisecond.
pub const MAX_OPERATIONS: u64 = 1_200;
/// The deepest nesting of script function calls.
pub const MAX_CALL_LEVELS: usize = 8;
/// The longest string, array and map a script can build.
pub const MAX_STRING: usize = 256;
pub const MAX_ARRAY: usize = 64;
pub const MAX_MAP: usize = 32;
/// The most telegrams one call can send.
pub const MAX_SENDS: u32 = 4;
/// The most telegrams a script can send in one second of the uptime, which
/// the runtime has from set_now: a script answering its own telegrams sends
/// at most this many.
pub const MAX_RATE: u32 = 16;

/// The functions every script defines, with their number of parameters.
pub const FUNCTIONS: [(&str, usize); 4] =
    [("on_info", 0), ("on_init", 0), ("on_telegram", 1), ("on_time", 1)];

/// The keys of the map on_info returns.
pub const INFO_KEYS: [&str; 6] = ["name", "author", "date", "compiled", "version", "hash"];

/// What the scripts reach of the firmware.
pub trait Host {
    /// Queues tg for the bus and reports whether it was taken.
    fn send(&mut self, tg: &Telegram) -> bool;
    /// Prints a line of a script's print or debug.
    fn print(&mut self, line: &str);
}

/// The state the engine's functions share with the runtime.
struct Shared {
    host: Box<dyn Host>,
    /// The sends of the call in progress, and how many it may make: at
    /// most MAX_SENDS, and fewer near MAX_RATE.
    sends: u32,
    allowed: u32,
    /// The sends refused since the runtime was made: by the host, or over
    /// the limits.
    refused: u32,
}

/// The engine of the scripts, with the functions they can call.
pub struct Runtime {
    engine: Engine,
    shared: Rc<RefCell<Shared>>,
    /// The second of the uptime, for MAX_RATE.
    now: core::cell::Cell<u32>,
}

impl Runtime {
    /// Makes the engine: Rhai's bare engine, without its packages, with the
    /// limits above and the functions of the scripts, which reach host.
    pub fn new(host: impl Host + 'static) -> Runtime {
        let shared = Rc::new(RefCell::new(Shared {
            host: Box::new(host),
            sends: 0,
            allowed: 0,
            refused: 0,
        }));
        let mut engine = Engine::new_raw();
        engine
            .set_max_operations(MAX_OPERATIONS)
            .set_max_call_levels(MAX_CALL_LEVELS)
            .set_max_string_size(MAX_STRING)
            .set_max_array_size(MAX_ARRAY)
            .set_max_map_size(MAX_MAP);
        register(&mut engine, &shared);
        Runtime {
            engine,
            shared,
            now: core::cell::Cell::new(0),
        }
    }

    /// Sets the second of the uptime that the calls count their sends in.
    pub fn set_now(&self, secs: u32) {
        self.now.set(secs);
    }

    /// The sends refused since the runtime was made.
    pub fn refused(&self) -> u32 {
        self.shared.borrow().refused
    }
}

/// Registers the functions of the scripts on engine.
fn register(engine: &mut Engine, shared: &Rc<RefCell<Shared>>) {
    // The bare engine has the hooks of print and debug, not the functions.
    let s = shared.clone();
    engine.register_fn("print", move |v: Dynamic| {
        s.borrow_mut().host.print(&v.to_string())
    });
    let s = shared.clone();
    engine.register_fn("debug", move |v: Dynamic| {
        s.borrow_mut().host.print(&format!("{v:?}"))
    });
    // Of the blob package, only the length, and blob(array) to make one.
    engine.register_fn("len", |b: &mut Blob| b.len() as INT);
    engine.register_fn("blob", |a: rhai::Array| -> Result<Blob, Box<EvalAltResult>> {
        a.iter()
            .map(|v| match v.as_int() {
                Ok(x) if (0..=255).contains(&x) => Ok(x as u8),
                _ => Err(format!("blob: {v} is not a byte").into()),
            })
            .collect()
    });

    engine.register_fn("ga", |s: &str| -> Result<INT, Box<EvalAltResult>> {
        GroupAddress::parse(s)
            .map(|g| g.0 as INT)
            .map_err(|e| format!("ga({s:?}): {e}").into())
    });
    engine.register_fn("ia", |s: &str| -> Result<INT, Box<EvalAltResult>> {
        IndividualAddress::parse(s)
            .map(|a| a.0 as INT)
            .map_err(|e| format!("ia({s:?}): {e}").into())
    });
    engine.register_fn("ga_str", |g: INT| GroupAddress(g as u16).to_string());
    engine.register_fn("ia_str", |a: INT| IndividualAddress(a as u16).to_string());

    // Values as a telegram's data: from the APCI octet on.
    engine.register_fn("dpt1", |on: bool| -> Blob { vec![on as u8] });
    engine.register_fn("dpt9", |v: FLOAT| -> Blob {
        dpt::f16_pack(hundredths(v)).to_vec()
    });
    engine.register_fn("dpt1_of", |data: Blob| -> Result<bool, Box<EvalAltResult>> {
        match *data {
            [x] => Ok(x & 1 == 1),
            _ => Err(format!("dpt1_of: {} bytes, not 1", data.len()).into()),
        }
    });
    engine.register_fn("dpt9_of", |data: Blob| -> Result<FLOAT, Box<EvalAltResult>> {
        dpt::f16_unpack(&data)
            .map(|h| h as FLOAT / 100.0)
            .ok_or_else(|| format!("dpt9_of: {} bytes, not 3", data.len()).into())
    });

    let s = shared.clone();
    engine.register_fn("knx_write", move |g: INT, data: Blob| {
        send(&s, g, Apci::GroupValueWrite, &data)
    });
    let s = shared.clone();
    engine.register_fn("knx_read", move |g: INT| send(&s, g, Apci::GroupValueRead, &[0]));
    let s = shared.clone();
    engine.register_fn("knx_response", move |g: INT, data: Blob| {
        send(&s, g, Apci::GroupValueResponse, &data)
    });
}

/// Rounds v to hundredths, within i32.
fn hundredths(v: FLOAT) -> i32 {
    let h = v * 100.0;
    let h = if h < 0.0 { h - 0.5 } else { h + 0.5 };
    h.clamp(i32::MIN as FLOAT, i32::MAX as FLOAT) as i32
}

/// Sends a group value service with data to group g, if the call may send
/// one more and the host takes it.
fn send(shared: &RefCell<Shared>, g: INT, apci: Apci, data: &[u8]) -> Result<bool, Box<EvalAltResult>> {
    if !(1..=0xffff).contains(&g) {
        return Err(format!("group address {g} out of range").into());
    }
    let mut tg = Telegram {
        dest: g as u16,
        npci: Npci {
            addr_type: AddrType::Group,
            ..Npci::default()
        },
        apci,
        ..Telegram::default()
    };
    tg.ctrl.priority = Priority::Low;
    tg.set_data(data)
        .map_err(|_| -> Box<EvalAltResult> { format!("{} bytes of data, at most 15", data.len()).into() })?;
    if data.is_empty() {
        return Err("no data".into());
    }
    let mut s = shared.borrow_mut();
    if s.sends >= s.allowed {
        s.refused += 1;
        return Ok(false);
    }
    s.sends += 1;
    let ok = s.host.send(&tg);
    if !ok {
        s.refused += 1;
    }
    Ok(ok)
}

/// Returns tg as the map that on_telegram gets.
pub fn telegram_map(tg: &Telegram, own: bool) -> Dynamic {
    let mut m = Map::new();
    m.insert("src".into(), Dynamic::from_int(tg.src as INT));
    m.insert("dest".into(), Dynamic::from_int(tg.dest as INT));
    m.insert(
        "group".into(),
        Dynamic::from_bool(tg.npci.addr_type == AddrType::Group),
    );
    let apci = match tg.apci {
        Apci::GroupValueWrite => "write",
        Apci::GroupValueRead => "read",
        Apci::GroupValueResponse => "response",
        _ => "other",
    };
    m.insert("apci".into(), Dynamic::from(apci));
    let priority = match tg.ctrl.priority {
        Priority::System => "system",
        Priority::High => "high",
        Priority::Alarm => "alarm",
        Priority::Low => "low",
    };
    m.insert("priority".into(), Dynamic::from(priority));
    m.insert("hops".into(), Dynamic::from_int(tg.npci.routing as INT));
    m.insert("data".into(), Dynamic::from_blob(tg.data().to_vec()));
    m.insert("own".into(), Dynamic::from_bool(own));
    Dynamic::from_map(m)
}

/// What on_info reports.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Info {
    pub name: String,
    pub author: String,
    pub date: String,
    pub compiled: String,
    pub version: String,
    pub hash: String,
}

impl Info {
    /// Reads the map on_info returned: a string for each of INFO_KEYS.
    fn from(v: Dynamic) -> Result<Info, String> {
        let m = v
            .try_cast::<Map>()
            .ok_or_else(|| "on_info: not a map".to_string())?;
        let get = |k: &str| -> Result<String, String> {
            match m.get(k) {
                Some(v) if v.is_string() => Ok(v.clone().into_string().unwrap_or_default()),
                Some(_) => Err(format!("on_info: {k} is not a string")),
                None => Err(format!("on_info: no {k}")),
            }
        };
        Ok(Info {
            name: get("name")?,
            author: get("author")?,
            date: get("date")?,
            compiled: get("compiled")?,
            version: get("version")?,
            hash: get("hash")?,
        })
    }
}

/// A failed call of a script.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Error {
    /// The function called.
    pub call: &'static str,
    /// Rhai's message.
    pub text: String,
    /// The addresses of the instructions where it failed, innermost first,
    /// which the sidecar of the build turns into source lines.
    pub faults: Vec<usize>,
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "{}: {}", self.call, self.text)
    }
}

/// A loaded script: its program and its state.
pub struct Script {
    program: Program<'static>,
    /// The object map that on_init sets up and the calls get as this.
    state: Dynamic,
    /// Rhai's scope of the calls; the scripts keep nothing in it.
    rhai_scope: Scope<'static>,
    /// The second of the uptime of the last call, and the sends since.
    second: u32,
    sent: u32,
}

impl Script {
    /// Reads a program written by [`compile`], checking its format and ABI,
    /// and copies it out of bytes.
    pub fn load(bytes: &[u8]) -> Result<Script, ReadError> {
        let program = Program::read(bytes)?.into_owned();
        Ok(Script {
            program,
            state: Dynamic::from_map(Map::new()),
            rhai_scope: Scope::new(),
            second: 0,
            sent: 0,
        })
    }

    /// Calls name with args, on a VM of its own: a VM counts the operations
    /// of all its calls against the limit. eval_ast(false) skips the top
    /// level, which declares nothing; rewind_scope stays on, as Rhai Grain
    /// 1.27 panics on a script error with both off.
    /// With this, the call is a method of the state.
    fn call(
        &mut self,
        rt: &Runtime,
        name: &'static str,
        this: bool,
        args: impl rhai::FuncArgs,
    ) -> Result<Dynamic, Error> {
        let now = rt.now.get();
        if now != self.second {
            self.second = now;
            self.sent = 0;
        }
        {
            let mut s = rt.shared.borrow_mut();
            s.sends = 0;
            s.allowed = MAX_SENDS.min(MAX_RATE.saturating_sub(self.sent));
        }
        let mut vm = Vm::new(&rt.engine);
        let mut opts = CallFnOptions::new().eval_ast(false);
        if this {
            opts = opts.bind_this_ptr(&mut self.state);
        }
        let r = vm
            .call_fn_with_options::<Dynamic>(opts, &mut self.rhai_scope, &self.program, name, args)
            .map_err(|e| Error {
                call: name,
                text: e.to_string(),
                faults: vm.fault_trace().iter().map(|f| f.address).collect(),
            });
        self.sent += rt.shared.borrow().sends;
        r
    }

    /// Calls on_info.
    pub fn info(&mut self, rt: &Runtime) -> Result<Info, Error> {
        let v = self.call(rt, "on_info", false, ())?;
        Info::from(v).map_err(|text| Error {
            call: "on_info",
            text,
            faults: Vec::new(),
        })
    }

    /// Calls on_init on a new, empty state.
    pub fn init(&mut self, rt: &Runtime) -> Result<(), Error> {
        self.state = Dynamic::from_map(Map::new());
        self.call(rt, "on_init", true, ()).map(|_| ())
    }

    /// Calls on_telegram with tg, own telling the board's own.
    pub fn telegram(&mut self, rt: &Runtime, tg: &Telegram, own: bool) -> Result<(), Error> {
        self.call(rt, "on_telegram", true, (telegram_map(tg, own),))
            .map(|_| ())
    }

    /// Calls on_time with the uptime in seconds.
    pub fn time(&mut self, rt: &Runtime, uptime: u32) -> Result<(), Error> {
        let up = uptime.min(INT::MAX as u32) as INT;
        self.call(rt, "on_time", true, (up,)).map(|_| ())
    }

    /// The state, for a look at it.
    pub fn state(&self) -> &Dynamic {
        &self.state
    }
}
