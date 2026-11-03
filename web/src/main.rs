//! knixx lights: switch the lights of a KNX installation through a knixx
//! board, from a browser with Web Bluetooth.

mod app;
mod ble;

fn main() {
    console_error_panic_hook::set_once();
    leptos::mount::mount_to_body(app::App);
}
