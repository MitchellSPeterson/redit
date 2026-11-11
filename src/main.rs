#[allow(dead_code)] mod terminal;
mod target;

use gpui_kit::component::{
    ActiveTheme, Disableable, Sizable,
    button::{Button, ButtonVariants},
    input::{Editor, EditorState, Input, InputEvent, InputState},
};
use gpui_kit::*;
use gpui_kit::prelude::FluentBuilder as _;
use std::collections::{HashMap, HashSet};
use target::{Entry, Target};

actions!(redit, [Save]);

struct Tab {
    key: String, // path, or "diff: path"
    title: String,
    state: Entity<EditorState>,
    saved: String,
    dirty: bool,
    readonly: bool,
    _sub: Subscription,
}

struct Redit {
    target: Target,
    gen_: u64, // bumped on target switch; stale results are dropped
    busy: usize,
    msg: String,
    focus: FocusHandle,
    target_input: Entity<InputState>,
    commit_input: Entity<InputState>,
    dirs: HashMap<String, Vec<Entry>>,
    expanded: HashSet<String>,
    tabs: Vec<Tab>,
    active: usize,
    branch: Option<String>,
    status: Vec<(String, String)>,
}

fn join(parent: &str, name: &str) -> String {
    if parent.is_empty() { name.into() } else { format!("{parent}/{name}") }
}

impl Redit {
    fn new(arg: String, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let a = arg.clone();
        let target_input = cx.new(|cx| InputState::new(window, cx).default_value(a));
        cx.subscribe_in(&target_input, window, |this, _, ev: &InputEvent, window, cx| {
            if let InputEvent::PressEnter { .. } = ev {
                this.open_target(window, cx);
            }
        })
        .detach();
        let commit_input = cx.new(|cx| InputState::new(window, cx).placeholder("Commit message"));
        let mut s = Self {
            target: Target::parse(&arg),
            gen_: 0,
            busy: 0,
            msg: String::new(),
            focus: cx.focus_handle(),
            target_input,
            commit_input,
            dirs: HashMap::new(),
            expanded: HashSet::new(),
            tabs: vec![],
            active: 0,
            branch: None,
            status: vec![],
        };
        s.reload(window, cx);
        s
    }

    /// Run a blocking Target job on the background executor, then apply the result on the UI thread.
    fn bg<T: Send + 'static>(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
        job: impl FnOnce(Target) -> T + Send + 'static,
        done: impl FnOnce(&mut Self, T, &mut Window, &mut Context<Self>) + 'static,
    ) {
        let t = self.target.clone();
        let g = self.gen_;
        self.busy += 1;
        cx.notify();
        cx.spawn_in(window, async move |this, cx| {
            let r = cx.background_spawn(async move { job(t) }).await;
            this.update_in(cx, |this, w, cx| {
                this.busy -= 1;
                if this.gen_ == g {
                    done(this, r, w, cx);
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    fn set_msg(&mut self, m: impl Into<String>) {
        self.msg = m.into().split_whitespace().collect::<Vec<_>>().join(" ");
    }

    fn open_target(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let s = self.target_input.read(cx).value().to_string();
        self.target = Target::parse(s.trim());
        self.gen_ += 1;
        self.dirs.clear();
        self.expanded.clear();
        self.tabs.clear();
        self.active = 0;
        self.reload(window, cx);
    }

    fn reload(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.set_msg(format!("opened {}", self.target.label()));
        self.load_dir(String::new(), window, cx);
        self.refresh_git(window, cx);
    }

    fn load_dir(&mut self, rel: String, window: &mut Window, cx: &mut Context<Self>) {
        let r = rel.clone();
        self.bg(window, cx, move |t| t.list_dir(&r), move |this, res, _, _| match res {
            Ok(v) => {
                this.dirs.insert(rel, v);
            }
            Err(e) => {
                this.expanded.remove(&rel);
                this.set_msg(format!("error: list {rel}: {e}"));
            }
        });
    }

    fn click_dir(&mut self, rel: String, window: &mut Window, cx: &mut Context<Self>) {
        if !self.expanded.remove(&rel) {
            self.expanded.insert(rel.clone());
            if !self.dirs.contains_key(&rel) {
                self.load_dir(rel, window, cx);
            }
        }
        cx.notify();
    }

    fn new_tab(
        &mut self,
        key: String,
        title: String,
        text: String,
        readonly: bool,
        lang: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let t = text.clone();
        let state = cx.new(|cx| {
            let mut s = EditorState::new(window, cx).language(lang).default_value(t);
            if readonly {
                s.set_readonly(true, cx);
            }
            s
        });
        let k = key.clone();
        let sub = cx.subscribe(&state, move |this, st, ev: &InputEvent, cx| {
            if let InputEvent::Change = ev {
                // ponytail: O(n) compare per keystroke; fine for source files.
                let cur = st.read(cx).text().to_string();
                if let Some(t) = this.tabs.iter_mut().find(|t| t.key == k) {
                    t.dirty = cur != t.saved;
                }
                cx.notify();
            }
        });
        self.tabs.push(Tab { key, title, state, saved: text, dirty: false, readonly, _sub: sub });
        self.active = self.tabs.len() - 1;
    }

    fn open_file(&mut self, rel: String, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(i) = self.tabs.iter().position(|t| t.key == rel) {
            self.active = i;
            return cx.notify();
        }
        let r = rel.clone();
        self.bg(window, cx, move |t| t.read(&r), move |this, res, w, cx| match res {
            Ok(text) => {
                let title = rel.rsplit('/').next().unwrap_or(&rel).to_string();
                let ext = rel.rsplit_once('.').map(|x| x.1).unwrap_or("text").to_string();
                this.new_tab(rel, title, text, false, ext, w, cx);
            }
            Err(e) => this.set_msg(format!("error: open {rel}: {e}")),
        });
    }

    fn open_diff(&mut self, rel: String, window: &mut Window, cx: &mut Context<Self>) {
        let key = format!("diff: {rel}");
        if let Some(i) = self.tabs.iter().position(|t| t.key == key) {
            self.tabs.remove(i); // re-run for fresh diff
        }
        let r = rel.clone();
        self.bg(window, cx, move |t| t.git_diff(&r), move |this, res, w, cx| match res {
            Ok(d) => {
                let d = if d.trim().is_empty() { "(no diff)".to_string() } else { d };
                this.new_tab(key.clone(), key, d, true, "diff".into(), w, cx);
            }
            Err(e) => this.set_msg(format!("error: diff {rel}: {e}")),
        });
    }

    fn save(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(tab) = self.tabs.get(self.active) else { return };
        if tab.readonly {
            return;
        }
        let (key, text) = (tab.key.clone(), tab.state.read(cx).text().to_string());
        let (k, t2) = (key.clone(), text.clone());
        self.bg(window, cx, move |t| t.write(&k, &t2), move |this, res, w, cx| match res {
            Ok(()) => {
                if let Some(t) = this.tabs.iter_mut().find(|t| t.key == key) {
                    let cur = t.state.read(cx).text().to_string();
                    t.dirty = cur != text;
                    t.saved = text;
                }
                this.set_msg(format!("saved {key}"));
                this.refresh_git(w, cx);
            }
            Err(e) => this.set_msg(format!("error: save {key}: {e}")),
        });
    }

    fn refresh_git(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.bg(
            window,
            cx,
            |t| t.git_branch().map(|b| (b, t.git_status())),
            |this, res, _, _| match res {
                None => (this.branch, this.status) = (None, vec![]),
                Some((b, st)) => {
                    this.branch = Some(b);
                    match st {
                        Ok(s) => this.status = s,
                        Err(e) => this.set_msg(format!("error: git status: {e}")),
                    }
                }
            },
        );
    }

    fn git_op(
        &mut self,
        name: &'static str,
        window: &mut Window,
        cx: &mut Context<Self>,
        job: impl FnOnce(Target) -> Result<String, String> + Send + 'static,
    ) {
        self.set_msg(format!("git {name}..."));
        self.bg(window, cx, job, move |this, res, w, cx| {
            match res {
                Ok(o) => this.set_msg(format!("git {name}: {o}")),
                Err(e) => this.set_msg(format!("error: git {name}: {e}")),
            }
            this.refresh_git(w, cx);
        });
    }

    fn commit(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let m = self.commit_input.read(cx).value().to_string();
        if m.trim().is_empty() {
            return self.set_msg("error: empty commit message");
        }
        self.commit_input.update(cx, |s, cx| s.set_value("", window, cx));
        self.git_op("commit", window, cx, move |t| t.git_commit_all(&m));
    }

    fn rows(&self, parent: &str, depth: usize, out: &mut Vec<(String, bool, usize, bool)>) {
        for e in self.dirs.get(parent).into_iter().flatten() {
            let p = join(parent, &e.name);
            let open = self.expanded.contains(&p);
            out.push((p.clone(), e.is_dir, depth, open));
            if e.is_dir && open {
                self.rows(&p, depth + 1, out);
            }
        }
    }
}

impl Render for Redit {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let th = cx.theme();
        let (border, muted, hover) = (th.border, th.muted_foreground, th.accent);
        let mut rows = vec![];
        self.rows("", 0, &mut rows);

        let tree = div().id("tree").flex_1().min_h_0().overflow_y_scroll().children(
            rows.into_iter().enumerate().map(|(i, (path, is_dir, depth, open))| {
                let name = path.rsplit('/').next().unwrap_or(&path).to_string();
                let icon = if is_dir { if open { "v " } else { "> " } } else { "  " };
                div()
                    .id(("row", i))
                    .pl(px(8.0 + 12.0 * depth as f32))
                    .py(px(2.0))
                    .text_sm()
                    .cursor_pointer()
                    .hover(move |s| s.bg(hover))
                    .child(format!("{icon}{name}"))
                    .on_click(cx.listener(move |this, _, w, cx| {
                        if is_dir {
                            this.click_dir(path.clone(), w, cx)
                        } else {
                            this.open_file(path.clone(), w, cx)
                        }
                    }))
            }),
        );

        let tabs = div().flex().flex_row().border_b_1().border_color(border).overflow_x_hidden().children(
            self.tabs.iter().enumerate().map(|(i, t)| {
                let act = i == self.active;
                div()
                    .id(("tab", i))
                    .flex()
                    .flex_row()
                    .gap_2()
                    .px_3()
                    .py_1()
                    .text_sm()
                    .cursor_pointer()
                    .border_r_1()
                    .border_color(border)
                    .when(act, |d| d.bg(hover))
                    .child(format!("{}{}", if t.dirty { "● " } else { "" }, t.title))
                    .child(div().id(("close", i)).text_color(muted).child("×").on_click(
                        cx.listener(move |this, _, _, cx| {
                            this.tabs.remove(i);
                            this.active = this.active.min(this.tabs.len().saturating_sub(1));
                            cx.stop_propagation();
                            cx.notify();
                        }),
                    ))
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.active = i;
                        cx.notify();
                    }))
            }),
        );

        let editor = match self.tabs.get(self.active) {
            Some(t) => div().flex_1().min_h_0().child(Editor::new(&t.state).h_full()),
            None => div().flex_1().flex().items_center().justify_center().text_color(muted).child("Open a file from the sidebar"),
        };

        let git = {
            let panel = div().w(px(280.0)).flex().flex_col().border_l_1().border_color(border).p_2().gap_2();
            match self.branch.clone() {
                None => panel.text_color(muted).text_sm().child("Git: not a repository"),
                Some(b) => panel
                    .child(div().text_sm().child(format!("Git: {b}")))
                    .child(
                        div()
                            .flex()
                            .flex_row()
                            .gap_1()
                            .child(Button::new("refresh").small().label("Refresh").on_click(
                                cx.listener(|this, _, w, cx| this.refresh_git(w, cx)),
                            ))
                            .child(Button::new("pull").small().label("Pull").on_click(
                                cx.listener(|this, _, w, cx| this.git_op("pull", w, cx, |t| t.git_pull())),
                            ))
                            .child(Button::new("push").small().label("Push").on_click(
                                cx.listener(|this, _, w, cx| this.git_op("push", w, cx, |t| t.git_push())),
                            )),
                    )
                    .child(Input::new(&self.commit_input))
                    .child(
                        Button::new("commit")
                            .small()
                            .primary()
                            .label("Commit all")
                            .disabled(self.status.is_empty())
                            .on_click(cx.listener(|this, _, w, cx| this.commit(w, cx))),
                    )
                    .child(
                        div().id("changes").flex_1().min_h_0().overflow_y_scroll().children(
                            self.status.iter().enumerate().map(|(i, (xy, p))| {
                                let (p2, untracked) = (p.clone(), xy == "??");
                                div()
                                    .id(("chg", i))
                                    .text_xs()
                                    .py(px(2.0))
                                    .cursor_pointer()
                                    .hover(move |s| s.bg(hover))
                                    .child(format!("{xy} {p}"))
                                    .on_click(cx.listener(move |this, _, w, cx| {
                                        if untracked {
                                            this.open_file(p2.clone(), w, cx)
                                        } else {
                                            this.open_diff(p2.clone(), w, cx)
                                        }
                                    }))
                            }),
                        ),
                    ),
            }
        };

        div()
            .id("root")
            .key_context("Redit")
            .track_focus(&self.focus)
            .on_action(cx.listener(|this, _: &Save, w, cx| this.save(w, cx)))
            .size_full()
            .flex()
            .flex_col()
            .bg(cx.theme().background)
            .text_color(cx.theme().foreground)
            .child(
                div().flex().flex_row().gap_2().p_2().border_b_1().border_color(border)
                    .child(div().flex_1().child(Input::new(&self.target_input)))
                    .child(Button::new("open").label("Open").on_click(
                        cx.listener(|this, _, w, cx| this.open_target(w, cx)),
                    )),
            )
            .child(
                div()
                    .flex_1()
                    .min_h_0()
                    .flex()
                    .flex_row()
                    .child(div().w(px(240.0)).flex().flex_col().border_r_1().border_color(border).child(tree))
                    .child(div().flex_1().min_w_0().flex().flex_col().child(tabs).child(editor))
                    .child(git),
            )
            .child(
                div().flex().flex_row().gap_4().px_2().py_1().text_xs().text_color(muted).border_t_1().border_color(border)
                    .child(self.target.label())
                    .child(div().flex_1().truncate().child(self.msg.clone()))
                    .when(self.busy > 0, |d| d.child("working...")),
            )
    }
}

fn main() {
    let arg = std::env::args().nth(1).unwrap_or_else(|| ".".into());
    gpui_kit::application().run(move |cx| {
        gpui_kit::init(cx);
        cx.bind_keys([KeyBinding::new("cmd-s", Save, None)]);
        cx.activate(true);
        let a = arg.clone();
        gpui_kit::open_window(WindowOptions::default(), cx, move |window, cx| {
            let v = cx.new(|cx| Redit::new(a, window, cx));
            let f = v.read(cx).focus.clone();
            f.focus(window, cx);
            v
        })
        .expect("failed to open window");
    });
}
