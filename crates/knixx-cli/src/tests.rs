use super::*;
use knixx_console::lineedit::{Editor, MAX_LINE, Sink};
use knixx_device::thunder::{MAX_GROUPS, NUM_SWITCHES, Switch};
use std::collections::VecDeque;
use std::format;
use std::string::String;
use std::vec::Vec;

/// Fake records the commands sent, up to a queue of cap, and the data saved.
struct Fake {
    dev: Device,
    flags: Flags,
    queue: VecDeque<Command>,
    cap: usize,
    saved: Option<Vec<u8>>,
    err: Option<&'static str>,
}

impl Fake {
    fn new(dev: Device, cap: usize) -> Self {
        Fake {
            dev,
            flags: Flags::new(),
            queue: VecDeque::new(),
            cap,
            saved: None,
            err: None,
        }
    }

    fn exec(&mut self, line: &str) -> (Action, String) {
        let mut w = String::new();
        let action = exec(&commands::<Fake>(), self, line, &mut w);
        (action, w)
    }

    fn out(&mut self, line: &str) -> String {
        self.exec(line).1
    }
}

impl Ctx for Fake {
    type SaveError = &'static str;

    fn device(&self) -> Device {
        self.dev
    }

    fn set_device(&mut self, dev: Device) {
        self.dev = dev;
    }

    fn send(&mut self, cmd: Command) -> bool {
        if self.queue.len() >= self.cap {
            return false;
        }
        self.queue.push_back(cmd);
        true
    }

    fn flags(&self) -> &Flags {
        &self.flags
    }

    fn save(&mut self, data: &[u8]) -> Result<(), &'static str> {
        if let Some(err) = self.err {
            return Err(err);
        }
        self.saved = Some(data.to_vec());
        Ok(())
    }
}

fn two_switches(individual: u16) -> Device {
    let switches: [Switch; NUM_SWITCHES] = [Switch::new("A", &[0x0002]), Switch::new("B", &[0x0003])];
    Device {
        individual,
        switches,
        ..Default::default()
    }
}

fn default_device() -> Device {
    config::parse(config::DEFAULT).unwrap()
}

fn format_ini(dev: &Device) -> Vec<u8> {
    let mut buf = [0; config::MAX_FILE];
    config::format(dev, &mut buf).to_vec()
}

#[test]
fn parse_switch_args() {
    let tests: [(&[&str], &str, bool, bool); 6] = [
        (&["A", "on"], "A", true, true),
        (&["b", "OFF"], "b", false, true),
        (&["A"], "", false, false),
        (&["A", "on", "now"], "", false, false),
        (&["A", "dim"], "", false, false),
        (&[], "", false, false),
    ];
    for (args, name, on, ok) in tests {
        let got = parse_switch(args);
        let want = ok.then_some((name, on));
        assert_eq!(got, want, "parse_switch({args:?})");
    }
}

#[test]
fn parse_on_off_arg() {
    let tests = [
        ("on", true, true),
        ("OFF", false, true),
        ("", false, false),
        ("bogus", false, false),
    ];
    for (arg, on, ok) in tests {
        assert_eq!(parse_on_off(arg), ok.then_some(on), "parse_on_off({arg:?})");
    }
}

#[test]
fn exec_lines() {
    let mut ctx = Fake::new(two_switches(0x7801), 2);

    let tests: [(&str, Action, &str, Option<Command>); 12] = [
        ("", Action::None, "", None),
        ("exit", Action::Exit, "", None),
        ("help", Action::None, "switch", None),
        ("bogus", Action::None, "unknown command \"bogus\"", None),
        (
            "switch b ON",
            Action::None,
            "",
            Some(Command::Switch { switch: 1, on: true }),
        ),
        ("switch C on", Action::None, "unknown switch: C", None),
        ("switch A", Action::None, SWITCH_USAGE, None),
        ("stat", Action::None, "", Some(Command::Stat)),
        ("stat now", Action::None, STAT_USAGE, None),
        ("sniff on", Action::None, "", None),
        ("sniff", Action::None, "sniff is on", None),
        ("sniff maybe", Action::None, SNIFF_USAGE, None),
    ];
    for (line, action, output, queued) in tests {
        let (got, w) = ctx.exec(line);
        assert_eq!(got, action, "{line:?}: action");
        assert!(
            w.contains(output) && !(output.is_empty() && !w.is_empty()),
            "{line:?}: output {w:?}, want {output:?}"
        );
        assert_eq!(ctx.queue.pop_front(), queued, "{line:?}: queued");
    }

    let mut w = String::new();
    for _ in 0..3 {
        w += &ctx.out("stat");
    }
    assert!(w.contains("tx queue full"), "full queue: output {w:?}");
}

#[test]
fn help_listing() {
    let mut ctx = Fake::new(two_switches(0), 1);
    let want = "  switch   usage: switch <A|B> <on|off>\n  \
                sniff    usage: sniff <on|off>\n  \
                stat     report the transceiver state and system status\n  \
                config   usage: config [dump | init | get <key> | set <key> <value>[,<value>...]]\n  \
                help     list the commands\n";
    assert_eq!(ctx.out("help"), want);
    assert_eq!(
        ctx.out("  bo\"gus  x"),
        "unknown command \"bo\\\"gus\", try help\n"
    );
}

#[test]
fn switch_helper() {
    let mut ctx = Fake::new(two_switches(0), 1);
    let mut w = String::new();

    switch(&mut ctx, "a", false, &mut w);
    let got = ctx.queue.pop_front();
    assert!(
        w.is_empty() && got == Some(Command::Switch { switch: 0, on: false }),
        "switch a off: output {w:?}, queued {got:?}"
    );
    switch(&mut ctx, "C", true, &mut w);
    assert!(
        w == "unknown switch: C\n" && ctx.queue.is_empty(),
        "switch C: output {w:?}, queued {}",
        ctx.queue.len()
    );
    w.clear();
    ctx.queue.push_back(Command::Stat);
    switch(&mut ctx, "B", true, &mut w);
    assert_eq!(w, "tx queue full\n", "full queue");
}

#[test]
fn config_cmds() {
    let mut ctx = Fake::new(default_device(), 1);

    let got = ctx.out("config");
    assert!(
        got.contains("device.individual = 7.8.1") && got.contains("A.group = ") && got.contains("B.group = "),
        "config: {got:?}"
    );
    assert_eq!(ctx.out("config get device.individual"), "7.8.1\n", "config get");

    for line in [
        "config get C.group",
        "config set A.group nope",
        "config set A.group",
        "config bogus",
    ] {
        let before = ctx.dev;
        let got = ctx.out(line);
        assert!(
            !got.is_empty() && ctx.dev == before && ctx.saved.is_none() && ctx.queue.is_empty(),
            "{line:?}: output {got:?}, dev {:?}, saved {:?}, queued {}",
            ctx.dev,
            ctx.saved,
            ctx.queue.len()
        );
    }

    assert_eq!(
        ctx.out("config set b.group 1/2/3"),
        "b.group = 1/2/3 (saved)\n",
        "config set"
    );
    assert_eq!(config::get(&ctx.dev, "B.group").unwrap(), "1/2/3");
    assert_eq!(ctx.saved.as_deref(), Some(&format_ini(&ctx.dev)[..]), "saved");
    assert_eq!(ctx.queue.pop_front(), Some(Command::Config(ctx.dev)), "queued");

    assert_eq!(
        ctx.out("config set A.group 0/0/1, 0/0/5,1/2/3"),
        "A.group = 0/0/1, 0/0/5, 1/2/3 (saved)\n",
        "config set list"
    );
    assert_eq!(
        ctx.out("config get a.group"),
        "0/0/1, 0/0/5, 1/2/3\n",
        "config get list"
    );
    ctx.queue.pop_front().unwrap();

    ctx.err = Some("flash unavailable");
    let got = ctx.out("config set A.group 0/0/9");
    assert_eq!(
        got, "A.group = 0/0/9 (not saved: flash unavailable)\n",
        "failed save"
    );
    ctx.queue.pop_front().unwrap();

    ctx.queue.push_back(Command::Stat);
    let before = ctx.dev;
    let got = ctx.out("config set A.group 0/0/7");
    assert!(
        got == "tx queue full\n" && ctx.dev == before,
        "full queue: output {got:?}, dev {:?}",
        ctx.dev
    );
}

/// Sink drops the line editor's echo.
struct Discard;

impl Sink for Discard {
    fn write(&mut self, _: &[u8]) {}
}

#[test]
fn config_dump() {
    let mut src = default_device();
    // The widest values: a switch bound to the most groups of the longest
    // form, which makes the longest dumped line.
    let groups: Vec<String> = (0..MAX_GROUPS).map(|i| format!("31/7/{}", 255 - i)).collect();
    let groups = groups.join(",");
    for (key, value) in [
        ("device.individual", "15.15.255"),
        ("A.group", groups.as_str()),
        ("humidity.group", "-"),
    ] {
        config::set(&mut src, key, value).unwrap();
    }

    let mut s = Fake::new(src, 1);
    let out = s.out("config dump");
    assert!(
        s.saved.is_none() && s.queue.is_empty(),
        "dump saved {:?}, queued {}",
        s.saved,
        s.queue.len()
    );
    let lines: Vec<&str> = out.strip_suffix('\n').unwrap_or(&out).split('\n').collect();
    assert_eq!(lines.len(), config::KEYS.len(), "dump {out:?}");
    for line in &lines {
        assert!(
            line.starts_with("config set ") && line.len() <= MAX_LINE,
            "dumped line {line:?} ({} bytes, max {MAX_LINE})",
            line.len()
        );
    }

    // Replay the dump through the console's line editor on a device with a
    // different configuration, including a sensor binding the source lacks.
    let mut dst = default_device();
    config::set(&mut dst, "humidity.group", "3/3/3").unwrap();
    let mut d = Fake::new(dst, 1);
    let mut ed = Editor::new("");
    for line in &lines {
        let mut edited = String::new();
        for b in line.bytes().chain(*b"\r") {
            if let Some(l) = ed.feed(b, &mut Discard) {
                edited = String::from_utf8(l.to_vec()).unwrap();
            }
        }
        let w = d.out(&edited);
        assert!(w.ends_with("(saved)\n"), "{line:?}: {w:?}");
        d.queue.pop_front().unwrap();
    }

    assert_eq!(d.dev, src, "replayed device");
    assert_eq!(d.saved.as_deref(), Some(&format_ini(&src)[..]), "replayed ini");
}

#[test]
fn config_init() {
    let def = default_device();
    let mut dev = default_device();
    for (key, value) in [
        ("device.individual", "1.2.3"),
        ("A.group", "1/1/1,1/1/2"),
        ("humidity.group", "-"),
    ] {
        config::set(&mut dev, key, value).unwrap();
    }
    let modified = dev;

    let mut ctx = Fake::new(dev, 1);

    let got = ctx.out("config init extra");
    assert!(
        got.starts_with("usage: config") && ctx.dev == modified,
        "config init extra: {got:?}"
    );

    ctx.queue.push_back(Command::Stat);
    let got = ctx.out("config init");
    assert!(
        got == "tx queue full\n" && ctx.dev == modified && ctx.saved.is_none(),
        "full queue: output {got:?}, dev {:?}, saved {:?}",
        ctx.dev,
        ctx.saved
    );
    ctx.queue.pop_front();

    let got = ctx.out("config init");
    assert!(
        got.contains("(saved)") && got.contains("device.individual = 7.8.1"),
        "config init: {got:?}"
    );
    assert_eq!(ctx.dev, def, "device");
    assert_eq!(ctx.saved.as_deref(), Some(config::DEFAULT), "saved");
    assert_eq!(ctx.queue.pop_front(), Some(Command::Config(def)), "queued");

    ctx.dev = modified;
    ctx.err = Some("flash unavailable");
    let got = ctx.out("config init");
    assert!(
        got.contains("not saved: flash unavailable") && ctx.dev == def,
        "failed save: output {got:?}, dev {:?}",
        ctx.dev
    );
}

#[test]
fn split_long_line() {
    let mut fields = [""; MAX_FIELDS];
    assert_eq!(split_fields(" \t a  bc\nd ", &mut fields), 3);
    assert_eq!(fields[..3], ["a", "bc", "d"]);

    let long: String = (0..MAX_FIELDS + 2).map(|_| "x ").collect();
    assert_eq!(split_fields(&long, &mut fields), MAX_FIELDS);
    assert_eq!(fields[MAX_FIELDS - 2], "x");
    assert_eq!(fields[MAX_FIELDS - 1], "x x x");
}
