//! Integrated terminal: portable-pty (child + pty) -> alacritty_terminal (VT emulation) -> gpui view.
//! `Core` is UI-free (and unit-tested); `TerminalView` only renders it and encodes input.

use alacritty_terminal::event::{Event, EventListener};
use alacritty_terminal::grid::{Dimensions, Scroll};
use alacritty_terminal::term::cell::{Cell, Flags};
use alacritty_terminal::term::test::TermSize;
use alacritty_terminal::term::{Config, Term, TermMode};
use alacritty_terminal::vte::ansi::{Color, Processor, Rgb};
use gpui_kit::component::ActiveTheme;
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use portable_pty::{Child, CommandBuilder, MasterPty, PtySize, native_pty_system};
use std::io::{Read, Write};
use std::sync::atomic::{AtomicBool, Ordering::SeqCst};
use std::sync::{Arc, Mutex};
use std::time::Duration;

type Writer = Arc<Mutex<Box<dyn Write + Send>>>;

/// Answers the emulator's own replies (cursor position reports etc.) back to the pty.
#[derive(Clone)]
struct Listener(Writer);

impl EventListener for Listener {
    fn send_event(&self, ev: Event) {
        if let Event::PtyWrite(s) = ev {
            let _ = self.0.lock().unwrap().write_all(s.as_bytes());
        }
    }
}

/// Screen contents at one instant: `rows[r][c]`, plus the cursor cell if visible.
pub struct Snap {
    pub rows: Vec<Vec<Cell>>,
    pub cursor: Option<(usize, usize)>,
}

struct Core {
    term: Arc<Mutex<Term<Listener>>>,
    master: Mutex<Box<dyn MasterPty + Send>>,
    writer: Writer,
    child: Mutex<Box<dyn Child + Send + Sync>>,
    dirty: Arc<AtomicBool>,
    exited: Arc<AtomicBool>,
}

impl Core {
    fn spawn(cmd: &std::process::Command, cols: u16, rows: u16) -> Result<Core, String> {
        fn e<E: ToString>(e: E) -> String {
            e.to_string()
        }
        let pair = native_pty_system()
            .openpty(PtySize { rows, cols, pixel_width: 0, pixel_height: 0 })
            .map_err(e)?;
        let mut b = CommandBuilder::new(cmd.get_program());
        b.args(cmd.get_args());
        if let Some(d) = cmd.get_current_dir() {
            b.cwd(d);
        }
        for (k, v) in cmd.get_envs() {
            match v {
                Some(v) => b.env(k, v),
                None => b.env_remove(k),
            }
        }
        b.env("TERM", "xterm-256color");
        b.env("COLORTERM", "truecolor");
        let child = pair.slave.spawn_command(b).map_err(e)?;
        drop(pair.slave);
        let mut reader = pair.master.try_clone_reader().map_err(e)?;
        let writer: Writer = Arc::new(Mutex::new(pair.master.take_writer().map_err(e)?));

        // 10k lines of scrollback (alacritty's default, stated explicitly).
        let cfg = Config { scrolling_history: 10_000, ..Config::default() };
        let term = Arc::new(Mutex::new(Term::new(
            cfg,
            &TermSize::new(cols as usize, rows as usize),
            Listener(writer.clone()),
        )));
        let (dirty, exited) = (Arc::new(AtomicBool::new(true)), Arc::new(AtomicBool::new(false)));

        let (t, d, x) = (term.clone(), dirty.clone(), exited.clone());
        std::thread::spawn(move || {
            let mut parser: Processor = Processor::new();
            let mut buf = [0u8; 16 * 1024];
            loop {
                match reader.read(&mut buf) {
                    Ok(n) if n > 0 => {
                        parser.advance(&mut *t.lock().unwrap(), &buf[..n]);
                        d.store(true, SeqCst); // view polls this once per frame: bytes coalesce
                    }
                    _ => break, // EOF or error: the child is gone
                }
            }
            parser.advance(&mut *t.lock().unwrap(), b"\r\n[process exited]\r\n");
            x.store(true, SeqCst);
            d.store(true, SeqCst);
        });

        Ok(Core {
            term,
            master: Mutex::new(pair.master),
            writer,
            child: Mutex::new(child),
            dirty,
            exited,
        })
    }

    fn write(&self, bytes: &[u8]) {
        if !self.exited.load(SeqCst) {
            let _ = self.writer.lock().unwrap().write_all(bytes);
        }
    }

    fn resize(&self, cols: u16, rows: u16) {
        self.term.lock().unwrap().resize(TermSize::new(cols as usize, rows as usize));
        let _ = self.master.lock().unwrap().resize(PtySize { rows, cols, pixel_width: 0, pixel_height: 0 });
    }

    fn snapshot(&self) -> (Snap, TermMode) {
        let term = self.term.lock().unwrap();
        let (cols, lines) = (term.columns(), term.screen_lines());
        let mut rows = vec![vec![Cell::default(); cols]; lines];
        let c = term.renderable_content();
        let off = c.display_offset as i32;
        for i in c.display_iter {
            let (r, col) = ((i.point.line.0 + off) as usize, i.point.column.0);
            if let Some(slot) = rows.get_mut(r).and_then(|row| row.get_mut(col)) {
                *slot = i.cell.clone();
            }
        }
        let show = c.mode.contains(TermMode::SHOW_CURSOR) && off == 0;
        let (cr, cc) = (c.cursor.point.line.0 as usize, c.cursor.point.column.0);
        (Snap { rows, cursor: show.then_some((cr, cc)) }, c.mode)
    }

    fn scroll(&self, s: Scroll) {
        self.term.lock().unwrap().scroll_display(s);
    }
}

impl Drop for Core {
    fn drop(&mut self) {
        let _ = self.child.lock().unwrap().kill(); // reader thread then hits EOF and exits
    }
}

// ---------- colors ----------

const ANSI: [u32; 16] = [
    0x000000, 0xcd3131, 0x0dbc79, 0xe5e510, 0x2472c8, 0xbc3fbc, 0x11a8cd, 0xe5e5e5, 0x666666, 0xf14c4c,
    0x23d18b, 0xf5f543, 0x3b8eea, 0xd670d6, 0x29b8db, 0xffffff,
];

fn hex(v: u32) -> Hsla {
    rgb(v).into()
}

fn indexed(i: u8) -> Hsla {
    match i {
        0..=15 => hex(ANSI[i as usize]),
        16..=231 => {
            let (v, lv) = (i as u32 - 16, |n: u32| if n == 0 { 0 } else { 55 + 40 * n });
            hex(lv(v / 36) << 16 | lv(v / 6 % 6) << 8 | lv(v % 6))
        }
        _ => {
            let g = 8 + 10 * (i as u32 - 232);
            hex(g << 16 | g << 8 | g)
        }
    }
}

fn resolve(c: Color, fg: Hsla, bg: Hsla) -> Hsla {
    match c {
        Color::Spec(Rgb { r, g, b }) => hex((r as u32) << 16 | (g as u32) << 8 | b as u32),
        Color::Indexed(i) => indexed(i),
        // 0..16 ANSI, 256 fg, 257 bg, 258 cursor, 259..267 dim ANSI, 267 bright fg, 268 dim bg
        Color::Named(n) => match n as usize {
            i @ 0..=15 => indexed(i as u8),
            i @ 259..=266 => indexed((i - 259) as u8),
            257 | 268 => bg,
            _ => fg,
        },
    }
}

// ---------- input ----------

/// Bytes to send for a keystroke, or None if the terminal doesn't handle it.
fn key_bytes(k: &Keystroke, mode: TermMode) -> Option<Vec<u8>> {
    let m = k.modifiers;
    let app = mode.contains(TermMode::APP_CURSOR);
    let csi = |c: char| if app { format!("\x1bO{c}") } else { format!("\x1b[{c}") };
    let s = match k.key.as_str() {
        "enter" => "\r".into(),
        "backspace" => "\x7f".into(),
        "tab" if m.shift => "\x1b[Z".into(),
        "tab" => "\t".into(),
        "escape" => "\x1b".into(),
        "up" => csi('A'),
        "down" => csi('B'),
        "right" => csi('C'),
        "left" => csi('D'),
        "home" => csi('H'),
        "end" => csi('F'),
        "pageup" => "\x1b[5~".into(),
        "pagedown" => "\x1b[6~".into(),
        "delete" => "\x1b[3~".into(),
        "insert" => "\x1b[2~".into(),
        key if m.control && !m.platform => {
            let c = key.chars().next().filter(|_| key.chars().count() == 1)?;
            return match c {
                'a'..='z' => Some(vec![c as u8 - b'a' + 1]),
                ' ' | '@' => Some(vec![0]),
                '[' => Some(vec![0x1b]),
                '\\' => Some(vec![0x1c]),
                ']' => Some(vec![0x1d]),
                _ => None,
            };
        }
        _ if m.platform => return None,
        _ => {
            let t = k.key_char.clone()?;
            return Some(if m.alt { [b"\x1b".as_slice(), t.as_bytes()].concat() } else { t.into_bytes() });
        }
    };
    Some(s.into_bytes())
}

// ---------- view ----------

pub struct TerminalView {
    core: Result<Arc<Core>, String>,
    focus: FocusHandle,
    size: (u16, u16), // cols, rows
    _poll: Task<()>,
}

impl TerminalView {
    /// Spawns `cmd` on a pty. Spawn failure is shown in the view, not returned.
    pub fn new(cmd: std::process::Command, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let focus = cx.focus_handle();
        focus.focus(window, cx);
        let size = (80, 24);
        let core = Core::spawn(&cmd, size.0, size.1).map(Arc::new);
        // ponytail: 16ms poll instead of a wakeup channel (no futures dep); upgrade to a channel if idle wakeups matter.
        let c = core.clone().ok();
        let poll = cx.spawn(async move |this, cx| {
            let Some(c) = c else { return };
            loop {
                cx.background_executor().timer(Duration::from_millis(16)).await;
                if c.dirty.swap(false, SeqCst) && this.update(cx, |_, cx| cx.notify()).is_err() {
                    return;
                }
                if c.exited.load(SeqCst) && !c.dirty.load(SeqCst) {
                    return;
                }
            }
        });
        Self { core, focus, size, _poll: poll }
    }

    fn on_key(&mut self, ev: &KeyDownEvent, cx: &mut Context<Self>) {
        let Ok(core) = &self.core else { return };
        let k = &ev.keystroke;
        let m = k.modifiers;
        let paste = if cfg!(target_os = "macos") { m.platform && k.key == "v" } else { m.control && m.shift && k.key == "v" };
        let (_, mode) = core.snapshot();
        let bytes = if paste {
            cx.read_from_clipboard().and_then(|c| c.text()).map(|t| {
                if mode.contains(TermMode::BRACKETED_PASTE) { format!("\x1b[200~{t}\x1b[201~").into_bytes() } else { t.into_bytes() }
            })
        } else {
            key_bytes(k, mode)
        };
        if let Some(b) = bytes {
            core.scroll(Scroll::Bottom);
            core.write(&b);
            cx.stop_propagation();
            cx.notify();
        }
    }

    fn on_scroll(&mut self, ev: &ScrollEvent, line_h: Pixels, cx: &mut Context<Self>) {
        let Ok(core) = &self.core else { return };
        let n = (ev.delta.pixel_delta(line_h).y / line_h).round() as i32; // + = wheel up
        if n == 0 {
            return;
        }
        let (_, mode) = core.snapshot();
        if mode.contains(TermMode::ALT_SCREEN) && mode.contains(TermMode::ALTERNATE_SCROLL) {
            // vim/less/htop have no scrollback: wheel becomes arrow keys.
            let key = match (n > 0, mode.contains(TermMode::APP_CURSOR)) {
                (true, true) => "\x1bOA",
                (true, false) => "\x1b[A",
                (false, true) => "\x1bOB",
                (false, false) => "\x1b[B",
            };
            core.write(key.repeat(n.unsigned_abs() as usize).as_bytes());
        } else {
            core.scroll(Scroll::Delta(n));
        }
        cx.notify();
    }

    fn resize(&mut self, cols: u16, rows: u16, cx: &mut Context<Self>) {
        if (cols, rows) != self.size && cols > 0 && rows > 0 {
            self.size = (cols, rows);
            if let Ok(c) = &self.core {
                c.resize(cols, rows);
            }
            cx.notify();
        }
    }
}

impl Focusable for TerminalView {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus.clone()
    }
}

#[derive(PartialEq)]
struct Style {
    fg: Hsla,
    bg: Option<Hsla>,
    bold: bool,
    italic: bool,
    underline: bool,
}

impl Render for TerminalView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let th = cx.theme();
        let (dfg, dbg, mono, fsize) = (th.foreground, th.background, th.mono_font_family.clone(), th.mono_font_size);
        let line_h = (fsize * 1.3).round();
        let cell_w = window
            .text_system()
            .advance(window.text_system().resolve_font(&font(mono.clone())), fsize, 'm')
            .map(|s| s.width)
            .unwrap_or(fsize * 0.6);

        let mut root = div()
            .id("terminal")
            .key_context("Terminal")
            .track_focus(&self.focus)
            .relative()
            .size_full()
            .overflow_hidden()
            .bg(dbg)
            .text_color(dfg)
            .font_family(mono)
            .text_size(fsize)
            .line_height(line_h)
            .on_mouse_down(MouseButton::Left, cx.listener(|this, _, w, cx| this.focus.focus(w, cx)))
            .on_key_down(cx.listener(|this, ev: &KeyDownEvent, _, cx| this.on_key(ev, cx)))
            .on_scroll(cx.listener(move |this, ev: &ScrollEvent, _, cx| this.on_scroll(ev, line_h, cx)))
            .child({
                // Measures the element; resize happens here so rows/cols track the real bounds.
                let view = cx.entity();
                canvas(
                    move |b, _, cx| {
                        let (c, r) = ((b.size.width / cell_w).floor(), (b.size.height / line_h).floor());
                        view.update(cx, |t, cx| t.resize(c as u16, r as u16, cx));
                    },
                    |_, _, _, _| {},
                )
                .absolute()
                .size_full()
            });

        let core = match &self.core {
            Ok(c) => c,
            Err(e) => return root.p_2().child(format!("[failed to start terminal: {e}]")),
        };
        let focused = self.focus.is_focused(window);
        let (snap, _) = core.snapshot();
        for (r, row) in snap.rows.iter().enumerate() {
            // Batch consecutive cells with equal style into one run (one div) per row.
            let mut runs: Vec<(Style, String, usize)> = vec![];
            for (c, cell) in row.iter().enumerate() {
                let (mut fg, mut bg) = (resolve(cell.fg, dfg, dbg), resolve(cell.bg, dfg, dbg));
                if cell.flags.contains(Flags::INVERSE) {
                    std::mem::swap(&mut fg, &mut bg);
                }
                if cell.flags.contains(Flags::DIM) {
                    fg.a *= 0.66;
                }
                if snap.cursor == Some((r, c)) && focused {
                    (fg, bg) = (dbg, dfg);
                }
                let st = Style {
                    fg,
                    bg: (bg != dbg).then_some(bg),
                    bold: cell.flags.intersects(Flags::BOLD),
                    italic: cell.flags.contains(Flags::ITALIC),
                    underline: cell.flags.intersects(Flags::ALL_UNDERLINES),
                };
                let ch = if cell.flags.intersects(Flags::HIDDEN | Flags::WIDE_CHAR_SPACER) { ' ' } else { cell.c };
                let spacer = cell.flags.contains(Flags::WIDE_CHAR_SPACER);
                match runs.last_mut() {
                    Some((s, text, n)) if *s == st => {
                        if !spacer {
                            text.push(ch);
                        }
                        *n += 1;
                    }
                    _ => runs.push((st, if spacer { String::new() } else { ch.to_string() }, 1)),
                }
            }
            root = root.child(div().flex().flex_row().h(line_h).flex_none().children(runs.into_iter().map(
                |(st, text, n)| {
                    div()
                        .flex_none()
                        .w(cell_w * n as f32)
                        .whitespace_nowrap()
                        .overflow_hidden()
                        .text_color(st.fg)
                        .when_some(st.bg, |d, b| d.bg(b))
                        .when(st.bold, |d| d.font_weight(FontWeight::BOLD))
                        .when(st.italic, |d| d.italic())
                        .when(st.underline, |d| d.text_decoration_1())
                        .child(text)
                },
            )));
        }
        root
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alacritty_terminal::vte::ansi::NamedColor;

    fn run(cmd: std::process::Command) -> Snap {
        let core = Core::spawn(&cmd, 80, 24).expect("spawn");
        for _ in 0..500 {
            if core.exited.load(SeqCst) {
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(core.exited.load(SeqCst), "process never exited");
        core.snapshot().0
    }

    fn line(s: &Snap, r: usize) -> String {
        s.rows[r].iter().map(|c| c.c).collect()
    }

    #[::core::prelude::v1::test]
    fn pty_to_grid_with_color() {
        let mut c = std::process::Command::new("printf");
        c.arg("hi\\033[31mred");
        let s = run(c);
        assert!(line(&s, 0).starts_with("hired"), "{:?}", line(&s, 0));
        assert_eq!(s.rows[0][0].fg, Color::Named(NamedColor::Foreground));
        for i in 2..5 {
            assert_eq!(s.rows[0][i].fg, Color::Named(NamedColor::Red));
        }
        assert!(line(&s, 1).contains("[process exited]"));
    }

    // Needs the throwaway sshd on 127.0.0.1:2222; run with `cargo test -- --ignored`.
    #[::core::prelude::v1::test]
    #[ignore]
    fn ssh_shell() {
        let t = crate::target::Target::Ssh { host: "ssh://mitchell@127.0.0.1:2222".into(), root: "/tmp".into() };
        let mut cmd = t.shell();
        cmd.env("SSH_AUTH_SOCK", format!("{}/.ssh/redit-test-agent.sock", std::env::var("HOME").unwrap()));
        let core = Core::spawn(&cmd, 80, 24).expect("spawn");
        std::thread::sleep(Duration::from_secs(3));
        core.write(b"echo hello-$((40+2)); pwd; exit\r");
        for _ in 0..500 {
            if core.exited.load(SeqCst) {
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        let s = core.snapshot().0;
        let all: Vec<String> = (0..24).map(|r| line(&s, r)).collect();
        assert!(all.iter().any(|l| l.contains("hello-42")), "{all:#?}");
        assert!(all.iter().any(|l| l.trim() == "/tmp"), "{all:#?}");
    }
}
