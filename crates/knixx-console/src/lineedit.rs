//! A minimal line editor for serial consoles: echo, backspace and a command
//! history browsed with the up and down keys.

/// The longest line a `u8` length holds. It fits a config set line for a
/// switch bound to the most group addresses, as printed by config dump, with
/// room for pasted lines.
pub const MAX_LINE: usize = 255;
pub const HISTORY_LEN: usize = 8;
/// The longest prompt.
pub const MAX_PROMPT: usize = 16;

const KEY_CTRL_N: u8 = 0x0e;
const KEY_CTRL_P: u8 = 0x10;
const KEY_BACKSPACE: u8 = 0x08;
const KEY_ESCAPE: u8 = 0x1b;
const KEY_DELETE: u8 = 0x7f;

/// Raw terminal output for echo and redraws.
pub trait Sink {
    /// Writes b. Console output is best effort: it cannot fail.
    fn write(&mut self, b: &[u8]);
}

/// Escape sequence decoder states.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Esc {
    None,
    /// After ESC.
    Start,
    /// After ESC [ or ESC O.
    Csi,
    /// After a parameter byte; the final byte is ignored.
    Param,
}

/// Editor turns terminal input into lines. All storage is fixed size.
pub struct Editor {
    prompt: &'static str,

    line: [u8; MAX_LINE],
    n: u8,

    /// The line being typed while browsing the history.
    draft: [u8; MAX_LINE],
    draft_n: u8,

    /// A ring of the most recent lines; the newest is at head-1.
    hist: [[u8; MAX_LINE]; HISTORY_LEN],
    hist_n: [u8; HISTORY_LEN],
    head: u8,
    count: u8,
    /// 0 while editing the draft and k when showing the k-th newest history
    /// entry.
    browse: u8,

    esc: Esc,
    last_cr: bool,

    /// Assembles a redrawn line so that it is written at once.
    redraw: [u8; 1 + MAX_PROMPT + MAX_LINE + 3],
}

impl Editor {
    /// Returns an empty editor that shows prompt, at most `MAX_PROMPT` bytes,
    /// on redraws.
    pub const fn new(prompt: &'static str) -> Self {
        assert!(prompt.len() <= MAX_PROMPT, "prompt too long");
        Editor {
            prompt,
            line: [0; MAX_LINE],
            n: 0,
            draft: [0; MAX_LINE],
            draft_n: 0,
            hist: [[0; MAX_LINE]; HISTORY_LEN],
            hist_n: [0; HISTORY_LEN],
            head: 0,
            count: 0,
            browse: 0,
            esc: Esc::None,
            last_cr: false,
            redraw: [0; 1 + MAX_PROMPT + MAX_LINE + 3],
        }
    }

    /// Processes one input byte, echoing to out. It returns the completed
    /// line when b ends one.
    pub fn feed<S: Sink + ?Sized>(&mut self, b: u8, out: &mut S) -> Option<&[u8]> {
        let last_cr = self.last_cr;
        self.last_cr = b == b'\r';

        match self.esc {
            Esc::Start => {
                self.esc = if b == b'[' || b == b'O' {
                    Esc::Csi
                } else {
                    Esc::None
                };
                return None;
            }
            Esc::Csi | Esc::Param => {
                match b {
                    0x30..=0x3f => self.esc = Esc::Param,
                    0x40..=0x7e => {
                        if self.esc == Esc::Csi {
                            match b {
                                b'A' => self.up(out),
                                b'B' => self.down(out),
                                _ => {}
                            }
                        }
                        self.esc = Esc::None;
                    }
                    _ => self.esc = Esc::None,
                }
                return None;
            }
            Esc::None => {}
        }

        match b {
            KEY_ESCAPE => self.esc = Esc::Start,
            // second half of a CR LF pair
            b'\n' if last_cr => {}
            b'\r' | b'\n' => return Some(self.enter()),
            KEY_CTRL_P => self.up(out),
            KEY_CTRL_N => self.down(out),
            KEY_BACKSPACE | KEY_DELETE => {
                if self.n > 0 {
                    self.n -= 1;
                    out.write(b"\x08 \x08");
                }
            }
            b' '..KEY_DELETE if (self.n as usize) < MAX_LINE => {
                self.line[self.n as usize] = b;
                self.n += 1;
                out.write(&[b]);
            }
            _ => {}
        }
        None
    }

    /// Drops the line being typed and any partial escape sequence, keeping
    /// the history. It readies the editor for input from a new terminal.
    pub fn discard(&mut self) {
        self.n = 0;
        self.browse = 0;
        self.esc = Esc::None;
        self.last_cr = false;
    }

    /// Records the current line in the history and hands it out.
    fn enter(&mut self) -> &[u8] {
        let n = self.n as usize;
        self.n = 0;
        self.browse = 0;
        let line = &self.line[..n];
        if !line.trim_ascii().is_empty() && (self.count == 0 || line != self.entry(1)) {
            let h = self.head as usize;
            self.hist[h][..n].copy_from_slice(line);
            self.hist_n[h] = n as u8;
            self.head = (self.head + 1) % HISTORY_LEN as u8;
            if (self.count as usize) < HISTORY_LEN {
                self.count += 1;
            }
        }
        &self.line[..n]
    }

    /// Returns the ring index of the k-th newest history line, 1 being the
    /// newest.
    fn index(&self, k: u8) -> usize {
        (self.head as usize + HISTORY_LEN - k as usize) % HISTORY_LEN
    }

    /// Returns the k-th newest history line, 1 being the newest.
    fn entry(&self, k: u8) -> &[u8] {
        let i = self.index(k);
        &self.hist[i][..self.hist_n[i] as usize]
    }

    fn up<S: Sink + ?Sized>(&mut self, out: &mut S) {
        if self.browse == self.count {
            return;
        }
        if self.browse == 0 {
            let n = self.n as usize;
            self.draft[..n].copy_from_slice(&self.line[..n]);
            self.draft_n = self.n;
        }
        self.browse += 1;
        self.show_entry(out);
    }

    fn down<S: Sink + ?Sized>(&mut self, out: &mut S) {
        if self.browse == 0 {
            return;
        }
        self.browse -= 1;
        if self.browse == 0 {
            let n = self.draft_n as usize;
            self.line[..n].copy_from_slice(&self.draft[..n]);
            self.n = self.draft_n;
            self.show(out);
        } else {
            self.show_entry(out);
        }
    }

    /// Replaces the line being edited with the history entry being browsed
    /// and redraws it.
    fn show_entry<S: Sink + ?Sized>(&mut self, out: &mut S) {
        let i = self.index(self.browse);
        let n = self.hist_n[i] as usize;
        self.line[..n].copy_from_slice(&self.hist[i][..n]);
        self.n = n as u8;
        self.show(out);
    }

    /// Redraws the line being edited. The redraw is handed to out in a
    /// single write, so that a USB serial port sends it in as few packets as
    /// possible instead of a burst of small ones.
    fn show<S: Sink + ?Sized>(&mut self, out: &mut S) {
        let parts: [&[u8]; 4] = [
            b"\r",
            self.prompt.as_bytes(),
            &self.line[..self.n as usize],
            b"\x1b[K",
        ];
        let mut len = 0;
        for p in parts {
            self.redraw[len..len + p.len()].copy_from_slice(p);
            len += p.len();
        }
        out.write(&self.redraw[..len]);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::format;
    use std::string::String;
    use std::vec::Vec;

    const UP: &str = "\x1b[A";
    const DOWN: &str = "\x1b[B";
    const LEFT: &str = "\x1b[D";
    const RIGHT: &str = "\x1b[C";
    const DEL: &str = "\x1b[3~";

    /// Records the output and counts the writes.
    #[derive(Default)]
    struct Out {
        buf: Vec<u8>,
        writes: usize,
    }

    impl Sink for Out {
        fn write(&mut self, b: &[u8]) {
            self.writes += 1;
            self.buf.extend_from_slice(b);
        }
    }

    impl Out {
        fn string(&self) -> String {
            String::from_utf8(self.buf.clone()).unwrap()
        }
        fn reset(&mut self) {
            self.buf.clear();
            self.writes = 0;
        }
    }

    fn new_editor() -> (std::boxed::Box<Editor>, Out) {
        (std::boxed::Box::new(Editor::new("> ")), Out::default())
    }

    /// Sends input to e and returns the lines it completed.
    fn feed(e: &mut Editor, out: &mut Out, input: &str) -> Vec<String> {
        let mut lines = Vec::new();
        for &b in input.as_bytes() {
            if let Some(line) = e.feed(b, out) {
                lines.push(String::from_utf8(line.to_vec()).unwrap());
            }
        }
        lines
    }

    /// Returns the line being edited, without ending it.
    fn current(e: &Editor) -> String {
        String::from_utf8(e.line[..e.n as usize].to_vec()).unwrap()
    }

    #[test]
    fn history_browse() {
        let (mut e, mut out) = new_editor();
        let got = feed(&mut e, &mut out, "switch A on\rstat\r");
        assert_eq!(got.join("|"), "switch A on|stat");
        feed(&mut e, &mut out, "con");

        let steps = [
            (UP, "stat"),
            (UP, "switch A on"),
            (UP, "switch A on"),
            (DOWN, "stat"),
            (DOWN, "con"),
            (DOWN, "con"),
        ];
        for (i, (key, want)) in steps.iter().enumerate() {
            out.reset();
            feed(&mut e, &mut out, key);
            assert_eq!(current(&e), *want, "step {i}");
        }

        out.reset();
        feed(&mut e, &mut out, UP);
        assert_eq!(out.string(), "\r> stat\x1b[K");
        assert_eq!(feed(&mut e, &mut out, "\r"), ["stat"]);
    }

    #[test]
    fn application_cursor_keys() {
        let (mut e, mut out) = new_editor();
        feed(&mut e, &mut out, "stat\r\x1bOA");
        assert_eq!(current(&e), "stat");
        feed(&mut e, &mut out, "\x1bOB");
        assert_eq!(current(&e), "");
    }

    #[test]
    fn ctrl_pn() {
        let (mut e, mut out) = new_editor();
        feed(&mut e, &mut out, "stat\r\x10");
        assert_eq!(current(&e), "stat", "Ctrl-P");
        feed(&mut e, &mut out, "\x0e");
        assert_eq!(current(&e), "", "Ctrl-N");
    }

    #[test]
    fn other_escapes_ignored() {
        let (mut e, mut out) = new_editor();
        let input = format!("st{LEFT}{RIGHT}{DEL}\x1b[1;5Aat\r");
        assert_eq!(feed(&mut e, &mut out, &input), ["stat"]);
        assert_eq!(out.string(), "stat");
    }

    #[test]
    fn history_filter() {
        let (mut e, mut out) = new_editor();
        feed(&mut e, &mut out, "stat\rstat\r\r   \rsniff on\rstat\r");
        let got: Vec<String> = (1..=e.count)
            .map(|k| String::from_utf8(e.entry(k).to_vec()).unwrap())
            .collect();
        assert_eq!(got.join("|"), "stat|sniff on|stat");
    }

    #[test]
    fn history_wraps() {
        let (mut e, mut out) = new_editor();
        for i in 0..HISTORY_LEN + 3 {
            feed(&mut e, &mut out, &format!("cmd {i}\r"));
        }
        for _ in 0..HISTORY_LEN + 2 {
            feed(&mut e, &mut out, UP);
        }
        assert_eq!(current(&e), "cmd 3", "oldest");
    }

    #[test]
    fn edit_recalled() {
        let (mut e, mut out) = new_editor();
        feed(&mut e, &mut out, &format!("switch A on\r{UP}\x7f\x7foff\r"));
        feed(&mut e, &mut out, &format!("{UP}{UP}"));
        assert_eq!(current(&e), "switch A on", "history entry changed");
        feed(&mut e, &mut out, DOWN);
        assert_eq!(current(&e), "switch A off", "newest");
    }

    #[test]
    fn line_basics() {
        let (mut e, mut out) = new_editor();
        let got = feed(&mut e, &mut out, "a\r\nb\ncx\x08d\r");
        assert_eq!(got.join("|"), "a|b|cd");
        assert!(
            out.string().contains("x\x08 \x08"),
            "backspace echo {:?}",
            out.string()
        );

        let got = feed(&mut e, &mut out, &format!("{}\r", "y".repeat(MAX_LINE + 5)));
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].len(), MAX_LINE, "long line");
    }

    #[test]
    fn redraw_single_write() {
        let mut e = std::boxed::Box::new(Editor::new(">>> "));
        let mut out = Out::default();
        feed(&mut e, &mut out, "switch A on\r");
        out.reset();
        feed(&mut e, &mut out, UP);
        assert_eq!(out.writes, 1, "redraw writes");
        assert_eq!(out.string(), "\r>>> switch A on\x1b[K");
    }

    #[test]
    fn discard() {
        let (mut e, mut out) = new_editor();
        feed(&mut e, &mut out, "first\r");
        feed(&mut e, &mut out, "half\x1b[");
        e.discard();
        assert_eq!(feed(&mut e, &mut out, "Anext\r"), ["Anext"]);
        feed(&mut e, &mut out, &format!("{UP}{UP}"));
        assert_eq!(current(&e), "first", "history lost");
    }
}
