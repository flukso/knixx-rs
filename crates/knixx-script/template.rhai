// A knixx thunder script, in Rhai: https://rhai.rs/book/
//
// Fill in on_info, then build and push it with
//
//     bolt script push <this file> --open 10
//
// The board calls on_init once after loading the script, on_telegram for
// every telegram on the bus and on_time every second. Each call may take
// at most 1200 operations (about 20 ms) and send at most 4 telegrams, and a
// script at most 16 telegrams a second; a call that fails is reported on
// the console, and a script that keeps failing is stopped.
//
// What a script can use:
//
//   ga("1/2/3") ia("1.1.5")       a group or individual address as a number
//   ga_str(g) ia_str(a)           the number as text
//   knx_write(g, data)            sends a group value write; data is a blob
//   knx_read(g)                   sends a group value read
//   knx_response(g, data)         sends a group value response
//   dpt1(on) dpt9(21.5)           a value as telegram data (DPT 1 and 9)
//   blob([0, 128])                telegram data of bytes, from the APCI
//                                 octet on: a DPT 5 value of 128, here
//   dpt1_of(data) dpt9_of(data)   the value of telegram data
//   data.len() data[i]            its length and its bytes
//   print(text)                   a line on the console
//
// The knx_ functions return false when the bus queue is full, the call has
// sent 4 telegrams already or the script 16 in this second.
//
// Scripts have while and loop, but no for loops.
//
// on_init, on_telegram and on_time are methods of the script's state, an
// object map that persists from call to call: they reach it as this, and
// what they set in it stays. A function of the script's own that uses the
// state is called as this.f(), and reaches it as this too.

fn on_info() {
    #{
        name: "example",
        author: "",
        date: "",
        // Filled in when the script is built; keep them as they are.
        compiled: "{{compiled}}",
        version: "{{version}}",
        hash: "{{hash}}",
    }
}

// The example follows the occupancy on 5/0/1 with light B on 0/0/2: it
// switches B on or off when its state does not match the occupancy, and
// switches it off 10 s after the last occupied, even while the occupancy
// stays on. The occupancy comes from this board's own ToF sensor, so it
// arrives with tg.own set.

fn on_init() {
    this.occupancy = ga("5/0/1");
    this.light = ga("0/0/2");
    // B's state, as the bus last showed it. A read only asks: knx_read
    // returns whether it was sent, and B answers later with a response,
    // which on_telegram takes. Until then, or if B does not answer reads,
    // B counts as off.
    this.on = false;
    knx_read(this.light);
    // The seconds after the last occupied after which B goes off. Functions
    // see only their parameters and this, so settings live in the state.
    this.hold = 10;
    // The uptime of the last on_time, and when B goes off; -1: not armed.
    this.now = 0;
    this.off_at = -1;
}

// Switches B to on, unless it is so already.
fn set_light(on) {
    if this.on != on {
        knx_write(this.light, dpt1(on));
        this.on = on;
    }
}

// tg: #{ src, dest, group, apci ("write", "read", "response" or "other"),
// priority, hops, data (a blob, from the APCI octet on), own (sent by this
// board) }
fn on_telegram(tg) {
    if tg.dest == this.light && (tg.apci == "write" || tg.apci == "response") {
        this.on = dpt1_of(tg.data);
    } else if tg.dest == this.occupancy && tg.apci == "write" {
        let occupied = dpt1_of(tg.data);
        this.set_light(occupied);
        if occupied {
            this.off_at = this.now + this.hold;
        }
    }
}

// uptime: seconds since boot.
fn on_time(uptime) {
    this.now = uptime;
    if this.off_at >= 0 && uptime >= this.off_at {
        this.off_at = -1;
        if this.on {
            print(`B off ${this.hold} s after the last occupied`);
            this.set_light(false);
        }
    }
}
