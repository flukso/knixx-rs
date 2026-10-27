//! cargo run -p knixx-host --example query -- <port> <command>
fn main() {
    let args: Vec<String> = std::env::args().collect();
    match knixx_host::usbserial::query(&args[1], &args[2], std::time::Duration::from_secs(1)) {
        Ok(out) => print!("{out}"),
        Err(e) => eprintln!("error: {e:?}"),
    }
}
