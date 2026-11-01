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

// scope is an object map that persists from call to call.
fn on_init(scope) {
    scope.light = ga("0/0/1");
    scope.on = false;
}

// tg: #{ src, dest, group, apci ("write", "read", "response" or "other"),
// priority, hops, data (a blob, from the APCI octet on), own (sent by this
// board) }
fn on_telegram(scope, tg) {
    if tg.own || tg.dest != scope.light {
        return;
    }
    if tg.apci == "write" {
        scope.on = dpt1_of(tg.data);
    } else if tg.apci == "read" {
        knx_response(scope.light, dpt1(scope.on));
    }
}

// uptime: seconds since boot.
fn on_time(scope, uptime) {
    if uptime % 3600 == 0 {
        print(`up ${uptime / 3600} h, light ${if scope.on { "on" } else { "off" }}`);
    }
}
