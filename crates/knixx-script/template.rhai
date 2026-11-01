// A knixx thunder script, in Rhai: https://rhai.rs/book/
//
// Fill in on_info, then build and upload it with
//
//     bolt script upload <this file>
//
// The board calls on_init once after loading the script, on_telegram for
// every telegram on the bus and on_time every second. Each call may take
// at most 1200 operations (about 20 ms) and send at most 4 telegrams; a
// call that fails is reported on the console, and a script that keeps
// failing is stopped.
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
// The knx_ functions return false when the bus queue is full or the call
// has sent 4 telegrams already.
//
// Scripts have while and loop, but no for loops.
//
// The scope is the script's state: on_init, on_telegram and on_time get the
// same one, and what they set in it stays. A function of the script's own
// that changes it is called on it, as scope.f(), and changes this; given
// the scope as an argument, f(scope), it would change a copy.

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

// scope is an object map that persists from call to call.
fn on_init(scope) {
    scope.occupancy = ga("5/0/1");
    scope.light = ga("0/0/2");
    // B's state, as the bus last showed it. A read only asks: knx_read
    // returns whether it was sent, and B answers later with a response,
    // which on_telegram takes. Until then, or if B does not answer reads,
    // B counts as off.
    scope.on = false;
    knx_read(scope.light);
    // The seconds after the last occupied after which B goes off. Functions
    // see only their parameters, so settings live in the scope too.
    scope.hold = 10;
    // The uptime of the last on_time, and when B goes off; -1: not armed.
    scope.now = 0;
    scope.off_at = -1;
}

// Switches B to on, unless it is so already. A function that changes the
// scope is called on it, scope.set_light(on), and reaches it as this: a
// scope passed as an argument to it would be a copy.
fn set_light(on) {
    if this.on != on {
        knx_write(this.light, dpt1(on));
        this.on = on;
    }
}

// tg: #{ src, dest, group, apci ("write", "read", "response" or "other"),
// priority, hops, data (a blob, from the APCI octet on), own (sent by this
// board) }
fn on_telegram(scope, tg) {
    if tg.dest == scope.light && (tg.apci == "write" || tg.apci == "response") {
        scope.on = dpt1_of(tg.data);
    } else if tg.dest == scope.occupancy && tg.apci == "write" {
        let occupied = dpt1_of(tg.data);
        scope.set_light(occupied);
        if occupied {
            scope.off_at = scope.now + scope.hold;
        }
    }
}

// uptime: seconds since boot.
fn on_time(scope, uptime) {
    scope.now = uptime;
    if scope.off_at >= 0 && uptime >= scope.off_at {
        scope.off_at = -1;
        if scope.on {
            print(`B off ${scope.hold} s after the last occupied`);
            scope.set_light(false);
        }
    }
}
