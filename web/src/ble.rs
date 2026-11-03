//! The board's knixx GATT service over Web Bluetooth. The rest of the app
//! sees Rust types only: a [`Board`] to send with, and callbacks for the
//! results, the group telegrams and the disconnection.

use core::future::IntoFuture;

use js_sys::{DataView, JsString, Uint8Array};
use knixx_knx::gatt::{self, Group, Send, Status};
use wasm_bindgen::JsCast;
use wasm_bindgen::prelude::*;
use web_sys::{
    BluetoothDevice, BluetoothLeScanFilterInit, BluetoothRemoteGattCharacteristic, BluetoothRemoteGattServer,
    BluetoothRemoteGattService, Event, RequestDeviceOptions,
};

/// The knixx service and its characteristics, as in the firmware's
/// knixx_gatt.
const SERVICE: &str = "6b6e7801-1b5d-4c8e-a2f0-3e5b9d8c7a10";
const SEND: &str = "6b6e7802-1b5d-4c8e-a2f0-3e5b9d8c7a10";
const RESULT: &str = "6b6e7803-1b5d-4c8e-a2f0-3e5b9d8c7a10";
const GROUP: &str = "6b6e7804-1b5d-4c8e-a2f0-3e5b9d8c7a10";
const RSSI: &str = "6b6e7805-1b5d-4c8e-a2f0-3e5b9d8c7a10";

/// A connected board.
pub struct Board {
    device: BluetoothDevice,
    send: BluetoothRemoteGattCharacteristic,
    // The event handlers live as long as the board.
    _handlers: Vec<Closure<dyn FnMut(Event)>>,
}

/// What a callback gets.
pub enum Note {
    Result(u8, Status),
    Group(Group),
    /// The link's signal strength as the board receives it, in dBm.
    Rssi(i8),
    Disconnected,
}

/// Why connecting failed, as a line for the user.
fn err(what: &str, e: JsValue) -> String {
    let msg = e
        .dyn_ref::<js_sys::Error>()
        .map(|e| String::from(e.message()))
        .or_else(|| e.as_string())
        .unwrap_or_else(|| format!("{e:?}"));
    format!("{what}: {msg}")
}

/// Awaits a promise, with what it was for on an error.
async fn call<T>(what: &str, p: impl IntoFuture<Output = Result<T, JsValue>>) -> Result<T, String> {
    p.await.map_err(|e| err(what, e))
}

/// Whether the browser has Web Bluetooth.
pub fn available() -> bool {
    web_sys::window()
        .map(|w| js_sys::Reflect::has(&w.navigator(), &"bluetooth".into()).unwrap_or(false))
        .unwrap_or(false)
}

/// The bytes of a characteristic's value in an event.
fn value(e: &Event) -> Option<Vec<u8>> {
    let ch: BluetoothRemoteGattCharacteristic = e.target()?.dyn_into().ok()?;
    let dv: DataView = ch.value()?;
    Some(
        Uint8Array::new_with_byte_offset_and_length(
            &dv.buffer(),
            dv.byte_offset() as u32,
            dv.byte_length() as u32,
        )
        .to_vec(),
    )
}

/// Asks the user to pick a board, connects to it and subscribes to the
/// results and the group telegrams, which go to note along with the
/// disconnection.
pub async fn connect(note: impl Fn(Note) + Clone + 'static) -> Result<Board, String> {
    let bt = web_sys::window()
        .ok_or("no window")?
        .navigator()
        .bluetooth()
        .ok_or("this browser has no Web Bluetooth: use Chrome or Edge")?;
    let filter = BluetoothLeScanFilterInit::new();
    filter.set_services(&[JsString::from(SERVICE)]);
    let opts = RequestDeviceOptions::new();
    opts.set_filters(&[filter]);
    let device: BluetoothDevice = call("no board chosen", bt.request_device(&opts)).await?;
    let server = device.gatt().ok_or("the board has no GATT server")?;
    let server: BluetoothRemoteGattServer = call("connect", server.connect()).await?;
    match setup(device, &server, note).await {
        // The board drops a central that is not paired, outside its pairing
        // window: the browser's error then only says the link is gone.
        Err(_) if !server.connected() => Err(DROPPED.into()),
        r => r,
    }
}

/// What the app says when the board dropped the link while it was set up.
const DROPPED: &str = "The board dropped the connection. If this device isn't paired with the board yet, \
    open the board's pairing window and connect again.";

/// Gets the service of a connected board and subscribes to its
/// notifications.
async fn setup(
    device: BluetoothDevice,
    server: &BluetoothRemoteGattServer,
    note: impl Fn(Note) + Clone + 'static,
) -> Result<Board, String> {
    let svc: BluetoothRemoteGattService =
        call("the knixx service", server.get_primary_service_with_str(SERVICE)).await?;
    let ch = |uuid: &'static str| {
        let svc = svc.clone();
        async move { call(uuid, svc.get_characteristic_with_str(uuid)).await }
    };
    let send = ch(SEND).await?;
    let result = ch(RESULT).await?;
    let group = ch(GROUP).await?;

    let mut handlers = Vec::new();
    let n = note.clone();
    let on_result = Closure::<dyn FnMut(Event)>::new(move |e: Event| {
        if let Some(Ok((seq, st))) = value(&e).map(|v| gatt::decode_result(&v)) {
            n(Note::Result(seq, st));
        }
    });
    result
        .add_event_listener_with_callback("characteristicvaluechanged", on_result.as_ref().unchecked_ref())
        .map_err(|e| err("result", e))?;
    handlers.push(on_result);
    let n = note.clone();
    let on_group = Closure::<dyn FnMut(Event)>::new(move |e: Event| {
        if let Some(Ok(g)) = value(&e).map(|v| Group::decode(&v)) {
            n(Note::Group(g));
        }
    });
    group
        .add_event_listener_with_callback("characteristicvaluechanged", on_group.as_ref().unchecked_ref())
        .map_err(|e| err("group", e))?;
    handlers.push(on_group);
    // A board of an earlier firmware has no rssi characteristic.
    let rssi = ch(RSSI).await.ok();
    if let Some(r) = &rssi {
        let n = note.clone();
        let on_rssi = Closure::<dyn FnMut(Event)>::new(move |e: Event| {
            if let Some(&[v]) = value(&e).as_deref() {
                n(Note::Rssi(v as i8));
            }
        });
        r.add_event_listener_with_callback("characteristicvaluechanged", on_rssi.as_ref().unchecked_ref())
            .map_err(|e| err("rssi", e))?;
        handlers.push(on_rssi);
    }
    let n = note;
    let on_gone = Closure::<dyn FnMut(Event)>::new(move |_| n(Note::Disconnected));
    device
        .add_event_listener_with_callback("gattserverdisconnected", on_gone.as_ref().unchecked_ref())
        .map_err(|e| err("disconnect", e))?;
    handlers.push(on_gone);

    // The first access to the encrypted characteristics makes the OS pair,
    // or encrypt with its bond.
    call("subscribe to the results", result.start_notifications()).await?;
    call("subscribe to the telegrams", group.start_notifications()).await?;
    if let Some(r) = rssi {
        call("subscribe to the signal strength", r.start_notifications()).await?;
    }
    Ok(Board {
        device,
        send,
        _handlers: handlers,
    })
}

impl Board {
    /// Writes a send; its result comes as a Note::Result.
    pub async fn send(&self, s: &Send) -> Result<(), String> {
        let mut buf = [0u8; gatt::MAX_VALUE];
        let n = s.encode(&mut buf);
        let p = self
            .send
            .write_value_with_response_with_u8_slice(&buf[..n])
            .map_err(|e| err("send", e))?;
        call("send", p).await.map(|_| ())
    }

    /// The name the board advertises.
    pub fn name(&self) -> String {
        self.device.name().unwrap_or_else(|| "knixx".into())
    }

    pub fn disconnect(&self) {
        if let Some(g) = self.device.gatt() {
            g.disconnect();
        }
    }
}
