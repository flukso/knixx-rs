use std::cell::RefCell;
use std::rc::Rc;
use std::string::String;
use std::vec::Vec;

use knixx_knx::tp1::{self, AddrType, Apci, Telegram};

use super::*;

const TEMPLATE: &str = include_str!("../template.rhai");

const META: Meta = Meta {
    compiled: "2026-11-01T12:00:00Z",
    version: "abc1234",
    hash: "0123456789abcdef0123456789abcdef",
};

/// Records what the scripts send and print; refuses sends while full.
#[derive(Default)]
struct Rec {
    sent: Vec<Telegram>,
    printed: Vec<String>,
    full: bool,
}

#[derive(Clone, Default)]
struct RecHost(Rc<RefCell<Rec>>);

impl Host for RecHost {
    fn send(&mut self, tg: &Telegram) -> bool {
        let mut r = self.0.borrow_mut();
        if r.full {
            return false;
        }
        r.sent.push(*tg);
        true
    }

    fn print(&mut self, line: &str) {
        self.0.borrow_mut().printed.push(line.into());
    }
}

/// Builds source and loads it, with a runtime that records.
fn load(source: &str) -> (Runtime, RecHost, Script) {
    let build = compile(source, &META).unwrap_or_else(|e| panic!("{e}"));
    let host = RecHost::default();
    let rt = Runtime::new(host.clone());
    let script = Script::load(&build.artifact).unwrap();
    (rt, host, script)
}

/// A telegram on the bus: a group service with data from another device.
fn tg(dest: &str, apci: Apci, data: &[u8]) -> Telegram {
    let mut t = Telegram {
        src: 0x1105,
        dest: knixx_knx::GroupAddress::parse(dest).unwrap().0,
        apci,
        ..Telegram::default()
    };
    t.npci.addr_type = AddrType::Group;
    t.npci.routing = 6;
    t.ctrl.priority = tp1::Priority::Low;
    t.set_data(data).unwrap();
    t
}

/// A script around its body: the four functions, on_telegram and on_time
/// as given.
fn script(telegram: &str, time: &str) -> String {
    std::format!(
        r#"fn on_info() {{ #{{ name: "t", author: "a", date: "d", compiled: "{{{{compiled}}}}", version: "{{{{version}}}}", hash: "{{{{hash}}}}" }} }}
fn on_init(scope) {{ scope.n = 0; }}
fn on_telegram(scope, tg) {{ {telegram} }}
fn on_time(scope, uptime) {{ {time} }}
"#
    )
}

#[test]
fn template_compiles_and_runs() {
    let build = compile(TEMPLATE, &META).unwrap();
    assert_eq!(
        build.info,
        Info {
            name: "example".into(),
            author: "".into(),
            date: "".into(),
            compiled: META.compiled.into(),
            version: META.version.into(),
            hash: META.hash.into(),
        }
    );
    let (rt, host, mut s) = load(TEMPLATE);
    s.init(&rt).unwrap();
    // A write of 0/0/1 sets the state; a read of it is answered with it.
    s.telegram(&rt, &tg("0/0/1", Apci::GroupValueWrite, &[1]), false)
        .unwrap();
    s.telegram(&rt, &tg("0/0/1", Apci::GroupValueRead, &[0]), false)
        .unwrap();
    // Another group, and the board's own telegram, change nothing.
    s.telegram(&rt, &tg("0/0/2", Apci::GroupValueWrite, &[0]), false)
        .unwrap();
    s.telegram(&rt, &tg("0/0/1", Apci::GroupValueWrite, &[0]), true)
        .unwrap();
    s.time(&rt, 3599).unwrap();
    s.time(&rt, 3600).unwrap();
    let r = host.0.borrow();
    assert_eq!(r.sent.len(), 1);
    let sent = r.sent[0];
    assert_eq!(sent.dest, 0x0001);
    assert_eq!(sent.apci, Apci::GroupValueResponse);
    assert_eq!(sent.data(), [1]);
    assert_eq!(sent.npci.addr_type, AddrType::Group);
    assert_eq!(r.printed, ["up 1 h, light on"]);
}

#[test]
fn scope_persists() {
    let (rt, _, mut s) = load(&script("scope.n += 1;", "scope.n += uptime;"));
    s.init(&rt).unwrap();
    for _ in 0..3 {
        s.telegram(&rt, &tg("1/2/3", Apci::GroupValueWrite, &[1]), false)
            .unwrap();
    }
    s.time(&rt, 10).unwrap();
    let n = s.scope().clone().flatten().try_cast::<Map>().unwrap()["n"].as_int();
    assert_eq!(n, Ok(13));
    // on_init starts over.
    s.init(&rt).unwrap();
    let n = s.scope().clone().flatten().try_cast::<Map>().unwrap()["n"].as_int();
    assert_eq!(n, Ok(0));
}

#[test]
fn telegram_fields() {
    let (rt, host, mut s) = load(&script(
        r#"print(`${ia_str(tg.src)} ${ga_str(tg.dest)} ${tg.group} ${tg.apci} ${tg.priority} ${tg.hops} ${tg.data.len()} ${tg.own} ${dpt9_of(tg.data)}`);"#,
        "",
    ));
    s.init(&rt).unwrap();
    let data = knixx_knx::dpt::f16_pack(2150);
    s.telegram(&rt, &tg("2/0/1", Apci::GroupValueWrite, &data), true)
        .unwrap();
    assert_eq!(
        host.0.borrow().printed,
        ["1.1.5 2/0/1 true write low 6 3 true 21.5"]
    );
}

#[test]
fn sends() {
    let (rt, host, mut s) = load(&script(
        r#"
        knx_write(ga("1/0/1"), dpt1(true));
        knx_write(ga("1/0/2"), dpt9(-12.34));
        knx_read(ga("1/0/3"));
        knx_response(ga("1/0/4"), blob([0, 128]));
        // Over the limit of four.
        scope.fifth = knx_write(ga("1/0/5"), dpt1(true));
        "#,
        "",
    ));
    s.init(&rt).unwrap();
    s.telegram(&rt, &tg("0/0/9", Apci::GroupValueWrite, &[1]), false)
        .unwrap();
    {
        let r = host.0.borrow();
        let got: Vec<_> = r
            .sent
            .iter()
            .map(|t| (t.dest, t.apci, t.data().to_vec()))
            .collect();
        assert_eq!(
            got,
            [
                (0x0801, Apci::GroupValueWrite, std::vec![1]),
                (
                    0x0802,
                    Apci::GroupValueWrite,
                    knixx_knx::dpt::f16_pack(-1234).to_vec()
                ),
                (0x0803, Apci::GroupValueRead, std::vec![0]),
                (0x0804, Apci::GroupValueResponse, std::vec![0, 128]),
            ]
        );
    }
    assert_eq!(rt.refused(), 1);
    // The count starts over at each call; a full queue refuses.
    host.0.borrow_mut().full = true;
    s.telegram(&rt, &tg("0/0/9", Apci::GroupValueWrite, &[1]), false)
        .unwrap();
    assert_eq!(rt.refused(), 6);
}

#[test]
fn errors_are_returned() {
    for (body, want) in [
        ("let x = 1 / 0;", "Division by zero"),
        ("nope();", "Function not found"),
        ("let a = [1]; a[5];", "out of bounds"),
        ("loop { scope.n += 1; }", "Too many operations"),
        (r#"ga("40/0/0");"#, "ga(\"40/0/0\")"),
        (
            "knx_write(ga(\"1/0/1\"), blob([0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0]));",
            "16 bytes of data, at most 15",
        ),
        ("blob([1, 256]);", "256 is not a byte"),
        ("knx_write(0, dpt1(true));", "out of range"),
        ("let s = \"x\"; while true { s += s; }", "Length of string"),
    ] {
        let (rt, _, mut s) = load(&script(body, ""));
        s.init(&rt).unwrap();
        let e = s
            .telegram(&rt, &tg("0/0/9", Apci::GroupValueWrite, &[1]), false)
            .unwrap_err();
        assert_eq!(e.call, "on_telegram");
        assert!(e.text.contains(want), "{body}: {e}");
        // The script still runs after an error.
        s.time(&rt, 1).unwrap();
    }
}

#[test]
fn compile_errors() {
    let missing = TEMPLATE.replace("fn on_time(scope, uptime)", "fn on_tick(scope, uptime)");
    let arity = TEMPLATE.replace("fn on_init(scope)", "fn on_init()");
    let for_loop = script("for i in 0..3 { scope.n += i; }", "");
    let syntax = script("let x = ;", "");
    let placeholder = TEMPLATE.replace("\"{{hash}}\"", "\"mine\"");
    for (source, want) in [
        (missing.as_str(), "fn on_time() with 2 parameter(s)"),
        (arity.as_str(), "fn on_init() with 1 parameter(s)"),
        (for_loop.as_str(), "for"),
        (syntax.as_str(), "line 3"),
        (
            placeholder.as_str(),
            "on_info: hash is \"mine\", not \"{{hash}}\"",
        ),
    ] {
        let e = compile(source, &META)
            .err()
            .map(|e| e.to_string())
            .unwrap_or_default();
        assert!(e.contains(want), "{want:?} not in {e:?}");
    }
}

#[test]
fn bad_bytecode() {
    let build = compile(TEMPLATE, &META).unwrap();
    assert!(Script::load(&build.artifact[..build.artifact.len() / 2]).is_err());
    let mut flipped = build.artifact.clone();
    let n = flipped.len();
    flipped[n / 2] ^= 0xff;
    // A flipped byte fails to load or still runs: it never panics.
    if let Ok(mut s) = Script::load(&flipped) {
        let rt = Runtime::new(RecHost::default());
        let _ = s.init(&rt);
    }
}
