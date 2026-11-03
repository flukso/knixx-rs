//! The app: a grid of lights, switched on their switch addresses (0/0/x)
//! and shown by their status addresses (0/1/x), which are read on
//! connecting and followed on the bus; a panel to send any group value; and
//! a log.

use std::collections::HashMap;
use std::rc::Rc;

use futures::channel::oneshot;
use futures::future::{Either, select};
use gloo_timers::future::TimeoutFuture;
use knixx_knx::dpt::{Dpt1001, Dpt5001, f16_pack, f16_unpack};
use knixx_knx::gatt::{Group, Op, Send, Status};
use knixx_knx::tp1::Priority;
use knixx_knx::{GroupAddress, IndividualAddress};
use leptos::prelude::*;
use leptos::task::spawn_local;

use crate::ble::{self, Board, Note};

/// The lights by default: 24, switched on 0/0/1 and on, their state read
/// from and reported on 0/1/1 and on.
const SWITCH: &str = "0/0/1";
const STATUS: &str = "0/1/1";
const COUNT: u16 = 24;
/// How long a light may take to answer its read.
const ANSWER_MS: u32 = 2000;
/// How long the board may take to report the outcome of a send.
const RESULT_MS: u32 = 3000;
/// The wait before a send the board was too busy for goes again.
const BUSY_MS: u32 = 100;
/// The log lines kept.
const LOG_LINES: usize = 100;

const KEY_SWITCH: &str = "knixx.switch";
const KEY_STATUS: &str = "knixx.status";
const KEY_COUNT: &str = "knixx.count";

#[derive(Clone, Copy, PartialEq, Eq)]
enum State {
    Unknown,
    NoAnswer,
    On,
    Off,
}

/// A light: written on its switch address, its state read from and
/// reported on its status address.
#[derive(Clone, Copy, PartialEq, Eq)]
struct Light {
    switch: u16,
    status: u16,
    state: State,
}

/// The datapoint types the send panel offers.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Dpt {
    Switch,
    Percent,
    Float,
}

/// The app's state; every field is a handle, so it is Copy.
#[derive(Clone, Copy)]
struct Ctrl {
    status: RwSignal<String>,
    connected: RwSignal<bool>,
    /// The link's RSSI, in dBm, as the board last reported it.
    rssi: RwSignal<Option<i8>>,
    busy: RwSignal<bool>,
    lights: RwSignal<Vec<Light>>,
    log: RwSignal<Vec<String>>,
    switch: RwSignal<String>,
    first_status: RwSignal<String>,
    count: RwSignal<u16>,
    board: StoredValue<Option<Rc<Board>>, LocalStorage>,
    pending: StoredValue<HashMap<u8, oneshot::Sender<Status>>, LocalStorage>,
    seq: StoredValue<u8>,
}

fn storage() -> Option<web_sys::Storage> {
    web_sys::window()?.local_storage().ok()?
}

fn load(key: &str) -> Option<String> {
    storage()?.get_item(key).ok()?
}

fn save(key: &str, v: &str) {
    if let Some(s) = storage() {
        let _ = s.set_item(key, v);
    }
}

fn parse_ga(s: &str) -> Option<u16> {
    GroupAddress::parse(s.trim()).ok().map(|g| g.0)
}

/// A group value as people read it, guessed from its length: a switch, a
/// percentage or a float.
fn describe(data: &[u8]) -> String {
    match data.len() {
        1 => Dpt1001::unpack(data).map(|d| d.to_string()).unwrap_or_default(),
        2 => Dpt5001::unpack(data).map(|d| d.to_string()).unwrap_or_default(),
        3 => f16_unpack(data)
            .map(|h| format!("{:.2}", h as f64 / 100.0))
            .unwrap_or_default(),
        _ => data
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect::<Vec<_>>()
            .join(" "),
    }
}

impl Ctrl {
    fn new() -> Ctrl {
        let ga = |key, default: &str| {
            load(key)
                .filter(|s| parse_ga(s).is_some())
                .unwrap_or(default.into())
        };
        let count = load(KEY_COUNT)
            .and_then(|s| s.parse().ok())
            .filter(|n| (1..=64).contains(n))
            .unwrap_or(COUNT);
        let c = Ctrl {
            status: RwSignal::new("not connected".into()),
            connected: RwSignal::new(false),
            rssi: RwSignal::new(None),
            busy: RwSignal::new(false),
            lights: RwSignal::new(Vec::new()),
            log: RwSignal::new(Vec::new()),
            switch: RwSignal::new(ga(KEY_SWITCH, SWITCH)),
            first_status: RwSignal::new(ga(KEY_STATUS, STATUS)),
            count: RwSignal::new(count),
            board: StoredValue::new_local(None),
            pending: StoredValue::new_local(HashMap::new()),
            seq: StoredValue::new(0),
        };
        c.reset_lights();
        c
    }

    fn log(&self, line: String) {
        self.log.update(|l| {
            l.insert(0, line);
            l.truncate(LOG_LINES);
        });
    }

    /// The lights of the ranges, all unknown.
    fn reset_lights(&self) {
        let switch = parse_ga(&self.switch.get_untracked()).unwrap_or(0);
        let status = parse_ga(&self.first_status.get_untracked()).unwrap_or(0);
        let n = self.count.get_untracked();
        self.lights.set(
            (0..n)
                .map(|i| Light {
                    switch: switch.wrapping_add(i),
                    status: status.wrapping_add(i),
                    state: State::Unknown,
                })
                .collect(),
        );
    }

    /// Sets the state of the light with the status address ga.
    fn set_state(&self, ga: u16, state: State, only_unknown: bool) {
        self.lights.update(|ls| {
            for l in ls.iter_mut().filter(|l| l.status == ga) {
                if !only_unknown || l.state == State::Unknown {
                    l.state = state;
                }
            }
        });
    }

    fn note(&self, n: Note) {
        match n {
            Note::Result(seq, st) => {
                if let Some(tx) = self.pending.try_update_value(|m| m.remove(&seq)).flatten() {
                    let _ = tx.send(st);
                }
            }
            Note::Group(g) => self.group(&g),
            Note::Rssi(v) => self.rssi.set(Some(v)),
            Note::Disconnected => {
                self.board.set_value(None);
                self.connected.set(false);
                self.rssi.set(None);
                self.status.set("disconnected".into());
                self.log("disconnected".into());
            }
        }
    }

    /// A group telegram from the bus: logged, and the light whose status it
    /// reports set.
    fn group(&self, g: &Group) {
        let op = match g.op {
            Op::Write => "write",
            Op::Read => "read",
            Op::Response => "response",
        };
        let value = if g.op == Op::Read {
            String::new()
        } else {
            format!(": {}", describe(g.data()))
        };
        let own = if g.own { " (this board)" } else { "" };
        self.log(format!(
            "{} → {} {op}{value}{own}",
            IndividualAddress(g.src),
            GroupAddress(g.dest)
        ));
        if g.op != Op::Read
            && let Ok(d) = Dpt1001::unpack(g.data())
        {
            self.set_state(g.dest, if d.0 { State::On } else { State::Off }, false);
        }
    }

    /// Sends a group value through the board and returns its outcome. A
    /// busy board gets it again a little later.
    async fn request(&self, op: Op, ga: u16, data: &[u8]) -> Result<Status, String> {
        loop {
            let board = self.board.get_value().ok_or("not connected")?;
            let seq = self.seq.try_update_value(|s| {
                *s = s.wrapping_add(1);
                *s
            });
            let seq = seq.unwrap_or(0);
            let send = Send::new(seq, op, ga, Priority::Low, data).map_err(|_| "bad value".to_string())?;
            let (tx, rx) = oneshot::channel();
            self.pending.update_value(|m| {
                m.insert(seq, tx);
            });
            board.send(&send).await?;
            let st = match select(rx, TimeoutFuture::new(RESULT_MS)).await {
                Either::Left((Ok(st), _)) => st,
                _ => {
                    self.pending.update_value(|m| {
                        m.remove(&seq);
                    });
                    return Err("no result from the board".into());
                }
            };
            if st != Status::Busy {
                return Ok(st);
            }
            TimeoutFuture::new(BUSY_MS).await;
        }
    }

    async fn connect(self) {
        self.busy.set(true);
        self.status.set("choose the board…".into());
        let me = self;
        match ble::connect(move |n| me.note(n)).await {
            Ok(b) => {
                let name = b.name();
                self.board.set_value(Some(Rc::new(b)));
                self.connected.set(true);
                self.status.set(format!("connected to {name}"));
                self.log(format!("connected to {name}"));
                self.poll().await;
            }
            Err(e) => {
                self.status.set(e.clone());
                self.log(e);
            }
        }
        self.busy.set(false);
    }

    /// Reads the status of each light, one at a time: the next only once the
    /// board reported the previous one sent. The answers come as group
    /// telegrams.
    async fn poll(self) {
        self.reset_lights();
        let gas: Vec<u16> = self.lights.get_untracked().iter().map(|l| l.status).collect();
        self.status.update(|s| s.push_str(", reading the lights…"));
        for ga in gas {
            match self.request(Op::Read, ga, &[]).await {
                Ok(Status::Confirmed) => {}
                Ok(st) => self.log(format!("read {}: {}", GroupAddress(ga), st.as_str())),
                Err(e) => {
                    self.log(format!("read {}: {e}", GroupAddress(ga)));
                    return;
                }
            }
            spawn_local(async move {
                TimeoutFuture::new(ANSWER_MS).await;
                self.set_state(ga, State::NoAnswer, true);
            });
        }
        self.status.update(|s| {
            if let Some(i) = s.find(", reading") {
                s.truncate(i);
            }
        });
    }

    async fn toggle(self, ga: u16, on: bool) {
        match self.request(Op::Write, ga, &Dpt1001(on).pack()).await {
            Ok(Status::Confirmed) => {}
            Ok(st) => self.log(format!("write {}: {}", GroupAddress(ga), st.as_str())),
            Err(e) => self.log(format!("write {}: {e}", GroupAddress(ga))),
        }
    }
}

#[component]
pub fn App() -> impl IntoView {
    let c = Ctrl::new();
    if !ble::available() {
        c.status
            .set("this browser has no Web Bluetooth: use Chrome or Edge on a desktop or Android".into());
    }

    let connect = move |_| {
        if c.connected.get_untracked() {
            if let Some(b) = c.board.get_value() {
                b.disconnect();
            }
        } else {
            spawn_local(c.connect());
        }
    };

    view! {
        <header>
            <h1>"knixx lights"</h1>
            <button class="primary" on:click=connect disabled=move || c.busy.get()>
                {move || if c.connected.get() { "Disconnect" } else { "Connect" }}
            </button>
            <Signal c />
            <p class="status">{move || c.status.get()}</p>
        </header>
        <main>
            <Lights c />
            <Panel c />
            <Log c />
        </main>
    }
}

/// The signal strength: bars and the RSSI in dBm, while connected.
#[component]
fn Signal(c: Ctrl) -> impl IntoView {
    // The bars lit from the RSSI: about -55 dBm and up is a strong link,
    // -85 dBm and below a weak one.
    let bars = move || match c.rssi.get() {
        None => 0,
        Some(v) if v >= -55 => 4,
        Some(v) if v >= -65 => 3,
        Some(v) if v >= -75 => 2,
        Some(v) if v >= -85 => 1,
        Some(_) => 0,
    };
    view! {
        <Show when=move || c.connected.get() && c.rssi.get().is_some()>
            <span class="signal" title="signal strength of the link, as the board receives it">
                <span class="value">{move || c.rssi.get().map(|v| format!("{v} dBm")).unwrap_or_default()}</span>
                <span class="bars">
                    {(1..=4)
                        .map(|i| {
                            view! {
                                <span class=move || { if bars() >= i { "bar lit" } else { "bar" } } style=format!("height: {}px", 4 + 3 * i)></span>
                            }
                        })
                        .collect_view()}
                </span>
            </span>
        </Show>
    }
}

#[component]
fn Lights(c: Ctrl) -> impl IntoView {
    view! {
        <section class="grid">
            <For
                each=move || c.lights.get()
                key=|l| (l.switch, l.status, l.state as u8)
                children=move |l| {
                    let (class, label) = match l.state {
                        State::Unknown => ("light unknown", "?"),
                        State::NoAnswer => ("light none", "no answer"),
                        State::On => ("light on", "on"),
                        State::Off => ("light off", "off"),
                    };
                    let click = move |_| {
                        if c.connected.get_untracked() {
                            spawn_local(c.toggle(l.switch, l.state != State::On));
                        }
                    };
                    view! {
                        <button
                            class=class
                            on:click=click
                            disabled=move || !c.connected.get()
                            title=format!("switch {}, status {}", GroupAddress(l.switch), GroupAddress(l.status))
                        >
                            <span class="ga">{GroupAddress(l.switch).to_string()}</span>
                            <span class="state">{label}</span>
                        </button>
                    }
                }
            />
        </section>
    }
}

#[component]
fn Panel(c: Ctrl) -> impl IntoView {
    let ga = RwSignal::new("0/0/1".to_string());
    let dpt = RwSignal::new(Dpt::Switch);
    let value = RwSignal::new("on".to_string());

    let send = move |op: Op| {
        let Some(g) = parse_ga(&ga.get_untracked()) else {
            c.log(format!("{:?} is not a group address", ga.get_untracked()));
            return;
        };
        let v = value.get_untracked();
        let data: Vec<u8> = match (op, dpt.get_untracked()) {
            (Op::Read, _) => Vec::new(),
            (_, Dpt::Switch) => match v.trim().to_lowercase().as_str() {
                "on" | "1" | "true" => Dpt1001(true).pack().to_vec(),
                "off" | "0" | "false" => Dpt1001(false).pack().to_vec(),
                _ => {
                    c.log(format!("{v:?} is not on or off"));
                    return;
                }
            },
            (_, Dpt::Percent) => match v.trim().trim_end_matches('%').trim().parse::<u8>() {
                Ok(p) if p <= 100 => Dpt5001(p).pack().to_vec(),
                _ => {
                    c.log(format!("{v:?} is not a percentage from 0 to 100"));
                    return;
                }
            },
            (_, Dpt::Float) => match v.trim().parse::<f64>() {
                Ok(f) if f.is_finite() => f16_pack((f * 100.0).round() as i32).to_vec(),
                _ => {
                    c.log(format!("{v:?} is not a number"));
                    return;
                }
            },
        };
        spawn_local(async move {
            let what = if op == Op::Read { "read" } else { "write" };
            match c.request(op, g, &data).await {
                Ok(st) => c.log(format!("{what} {}: {}", GroupAddress(g), st.as_str())),
                Err(e) => c.log(format!("{what} {}: {e}", GroupAddress(g))),
            }
        });
    };

    let apply = move |_| {
        for v in [c.switch.get_untracked(), c.first_status.get_untracked()] {
            if parse_ga(&v).is_none() {
                c.log(format!("{v:?} is not a group address"));
                return;
            }
        }
        save(KEY_SWITCH, &c.switch.get_untracked());
        save(KEY_STATUS, &c.first_status.get_untracked());
        save(KEY_COUNT, &c.count.get_untracked().to_string());
        if c.connected.get_untracked() {
            spawn_local(c.poll());
        } else {
            c.reset_lights();
        }
    };

    view! {
        <details>
            <summary>"Send a group value"</summary>
            <div class="row">
                <label>"Group" <input bind:value=ga size="9" /></label>
                <label>
                    "Type"
                    <select on:change=move |e| {
                        dpt.set(match event_target_value(&e).as_str() {
                            "5.001" => Dpt::Percent,
                            "9.001" => Dpt::Float,
                            _ => Dpt::Switch,
                        })
                    }>
                        <option value="1.001">"1.001 switch"</option>
                        <option value="5.001">"5.001 percent"</option>
                        <option value="9.001">"9.001 float"</option>
                    </select>
                </label>
                <label>"Value" <input bind:value=value size="7" /></label>
            </div>
            <div class="row">
                <button on:click=move |_| send(Op::Write) disabled=move || !c.connected.get()>"Write"</button>
                <button on:click=move |_| send(Op::Read) disabled=move || !c.connected.get()>"Read"</button>
            </div>
        </details>
        <details>
            <summary>"Lights"</summary>
            <div class="row">
                <label>"First switch" <input bind:value=c.switch size="9" /></label>
                <label>"First status" <input bind:value=c.first_status size="9" /></label>
                <label>
                    "Count"
                    <input
                        type="number"
                        min="1"
                        max="64"
                        prop:value=move || c.count.get().to_string()
                        on:change=move |e| {
                            if let Ok(n) = event_target_value(&e).parse::<u16>() {
                                c.count.set(n.clamp(1, 64));
                            }
                        }
                    />
                </label>
                <button on:click=apply>"Apply"</button>
            </div>
            <p class="hint">
                "Light n is switched on the n-th switch address and reports its state on the n-th status address, which the app reads on connecting: the actuator's status object needs its read flag."
            </p>
        </details>
    }
}

#[component]
fn Log(c: Ctrl) -> impl IntoView {
    view! {
        <section class="log">
            <h2>"Bus"</h2>
            <ul>{move || c.log.get().into_iter().map(|l| view! { <li>{l}</li> }).collect_view()}</ul>
        </section>
    }
}
