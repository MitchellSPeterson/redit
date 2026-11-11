//! SSH_ASKPASS mode: a tiny prompt window. Answer -> stdout, exit 0; cancel/close -> exit 1.

use gpui_kit::component::{
    ActiveTheme, Sizable,
    button::{Button, ButtonVariants},
    input::{Input, InputEvent, InputState},
};
use gpui_kit::*;
use std::io::Write;

actions!(redit_askpass, [Cancel]);

struct AskPass {
    prompt: String,
    input: Entity<InputState>,
    focus: FocusHandle,
}

impl AskPass {
    fn ok(&mut self, _: &mut Window, cx: &mut Context<Self>) {
        let ans = self.input.read(cx).value().to_string();
        let mut o = std::io::stdout();
        let _ = writeln!(o, "{ans}");
        let _ = o.flush();
        std::process::exit(0);
    }
}

impl Render for AskPass {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .key_context("Askpass")
            .track_focus(&self.focus)
            .on_action(|_: &Cancel, _, _| std::process::exit(1))
            .size_full()
            .flex()
            .flex_col()
            .gap_3()
            .p_4()
            .bg(cx.theme().background)
            .text_color(cx.theme().foreground)
            .child(div().text_sm().child(self.prompt.clone()))
            .child(Input::new(&self.input))
            .child(
                div().flex().flex_row().gap_2().justify_end()
                    .child(Button::new("cancel").small().label("Cancel").on_click(|_, _, _| std::process::exit(1)))
                    .child(Button::new("ok").small().primary().label("OK").on_click(cx.listener(|this, _, w, cx| this.ok(w, cx)))),
            )
    }
}

pub fn run_askpass(prompt: String) {
    gpui_kit::application().run(move |cx| {
        gpui_kit::init(cx);
        cx.bind_keys([KeyBinding::new("escape", Cancel, Some("Askpass"))]);
        cx.on_window_closed(|cx, _| {
            if cx.windows().is_empty() {
                std::process::exit(1);
            }
        })
        .detach();
        cx.activate(true);
        let plain = {
            let p = prompt.to_lowercase();
            p.contains("yes/no") || p.contains("fingerprint")
        };
        let opts = WindowOptions {
            window_bounds: Some(WindowBounds::Windowed(Bounds::centered(None, size(px(460.0), px(170.0)), cx))),
            titlebar: Some(TitlebarOptions { title: Some("redit — SSH".into()), ..Default::default() }),
            kind: WindowKind::PopUp,
            ..Default::default()
        };
        gpui_kit::open_window(opts, cx, move |window, cx| {
            let v = cx.new(|cx| {
                let input = cx.new(|cx| InputState::new(window, cx).masked(!plain));
                cx.subscribe_in(&input, window, |this: &mut AskPass, _, ev: &InputEvent, w, cx| {
                    if let InputEvent::PressEnter { .. } = ev {
                        this.ok(w, cx);
                    }
                })
                .detach();
                input.update(cx, |s, cx| s.focus(window, cx));
                AskPass { prompt, input, focus: cx.focus_handle() }
            });
            v
        })
        .expect("failed to open window");
    });
}
