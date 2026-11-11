mod lsp;
mod panels;
mod terminal;
mod target;

use gpui_kit::component::{
    ActiveTheme, Disableable, Icon, Selectable, StyledExt, Sizable, Theme, TitleBar,
    button::{Button, ButtonVariants},
    kbd::Kbd,
    spinner::Spinner,
    input::{Editor, EditorState, Input, InputEvent, InputState, Position},
    select::{Select, SelectEvent, SelectState},
};
use gpui_kit::*;
use gpui_kit::assets::IconName;
use gpui_kit::prelude::FluentBuilder as _;
use std::collections::{HashMap, HashSet};
use std::time::Duration;
use target::{Entry, Forward, SearchHit, Target};

actions!(redit, [Save, FocusSearch, ToggleBottom]);

struct Tab {
    key: String, // path, or "diff: path"
    title: String,
    state: Entity<EditorState>,
    saved: String,
    dirty: bool,
    readonly: bool,
    mtime: Option<i64>,
    _sub: Subscription,
}

struct Redit {
    target: Target,
    lsp: lsp::LspHandle,
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
    search_mode: bool,
    search_input: Entity<InputState>,
    hits: Vec<SearchHit>,
    polling: bool,
    host_select: Entity<SelectState<Vec<String>>>,
    port_input: Entity<InputState>,
    forwards: Vec<Forward>,
    bottom_open: bool,
    /// Slot for the terminal: set to `Some(entity.into())` and it renders in the bottom panel.
    bottom: Option<AnyView>,
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
        let search_input = cx.new(|cx| InputState::new(window, cx).placeholder("Search in files"));
        cx.subscribe_in(&search_input, window, |this, _, ev: &InputEvent, window, cx| {
            if let InputEvent::PressEnter { .. } = ev {
                this.run_search(window, cx);
            }
        })
        .detach();
        let port_input = cx.new(|cx| InputState::new(window, cx).placeholder("port or local:remote"));
        let host_select = cx.new(|cx| SelectState::new(target::ssh_hosts(), None, window, cx));
        cx.subscribe_in(&host_select, window, |this, _, ev: &SelectEvent<Vec<String>>, window, cx| {
            if let SelectEvent::Confirm(Some(h)) = ev {
                let v = format!("{h}:");
                this.target_input.update(cx, |s, cx| {
                    s.set_value(v, window, cx);
                    s.focus(window, cx);
                });
            }
        })
        .detach();
        // File watching: poll every 3s; the loop ends when the view is dropped.
        cx.spawn_in(window, async move |this, cx| {
            loop {
                cx.background_executor().timer(Duration::from_secs(3)).await;
                if this.update_in(cx, |this, w, cx| this.tick(w, cx)).is_err() {
                    break;
                }
            }
        })
        .detach();
        cx.observe_window_appearance(window, |_, w, cx| Theme::sync_system_appearance(Some(w), cx)).detach();
        let mut s = Self {
            target: Target::parse(&arg),
            lsp: lsp::LspHandle::new(Target::parse(&arg)),
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
            search_mode: false,
            search_input,
            hits: vec![],
            polling: false,
            host_select,
            port_input,
            forwards: vec![],
            bottom_open: false,
            bottom: None,
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
        self.lsp = lsp::LspHandle::new(self.target.clone()); // old servers shut down on drop
        self.gen_ += 1;
        self.dirs.clear();
        self.expanded.clear();
        self.tabs.clear();
        self.active = 0;
        self.hits.clear();
        self.stop_forwards(cx);
        self.bottom = None; // terminal belongs to the old target
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
        mtime: Option<i64>,
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
        self.tabs.push(Tab { key, title, state, saved: text, dirty: false, readonly, mtime, _sub: sub });
        self.active = self.tabs.len() - 1;
    }

    /// Open (or focus) a file; `line` is 0-based and moves the cursor there.
    fn open_file(&mut self, rel: String, line: Option<u32>, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(i) = self.tabs.iter().position(|t| t.key == rel) {
            self.active = i;
            self.goto_line(i, line, window, cx);
            return cx.notify();
        }
        let r = rel.clone();
        self.bg(
            window,
            cx,
            move |t| t.read(&r).map(|x| (x, t.mtimes(&[r]).ok().and_then(|m| m[0]))),
            move |this, res, w, cx| match res {
                Ok((text, mtime)) => {
                    let title = rel.rsplit('/').next().unwrap_or(&rel).to_string();
                    let ext = rel.rsplit_once('.').map(|x| x.1).unwrap_or("text").to_string();
                    this.new_tab(rel.clone(), title, text, false, ext, mtime, w, cx);
                    let me = cx.entity().downgrade();
                    let me2 = me.clone();
                    let open: lsp::OpenFn = std::rc::Rc::new(move |rel, range, w, app| {
                        let rel = rel.to_string();
                        let _ = me.update(app, |this, cx| this.open_file(rel, Some(range.start.line), w, cx));
                    });
                    let on_err = move |e: String, app: &mut App| {
                        let _ = me2.update(app, |this, cx| {
                            this.set_msg(format!("lsp: {e}"));
                            cx.notify();
                        });
                    };
                    // Err = no server for this extension: plain editor.
                    let _ = lsp::attach(&this.tabs[this.active].state, &this.lsp, &rel, open, on_err, cx);
                    this.goto_line(this.active, line, w, cx);
                }
                Err(e) => this.set_msg(format!("error: open {rel}: {e}")),
            },
        );
    }

    fn goto_line(&mut self, i: usize, line: Option<u32>, window: &mut Window, cx: &mut Context<Self>) {
        if let (Some(l), Some(t)) = (line, self.tabs.get(i)) {
            t.state.update(cx, |s, cx| s.set_cursor_position(Position::new(l, 0), window, cx));
        }
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
                this.new_tab(key.clone(), key, d, true, "diff".into(), None, w, cx);
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
        let lsp = self.lsp.clone();
        let job = move |t: Target| {
            t.write(&k, &t2)?;
            lsp.did_save(&k);
            Ok::<_, String>(t.mtimes(&[k]).ok().and_then(|m| m[0]))
        };
        self.bg(window, cx, job, move |this, res, w, cx| match res {
            Ok(mtime) => {
                if let Some(t) = this.tabs.iter_mut().find(|t| t.key == key) {
                    t.mtime = mtime;
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

    fn run_search(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let q = self.search_input.read(cx).value().to_string();
        if q.is_empty() {
            return;
        }
        self.set_msg(format!("searching {q}..."));
        self.bg(window, cx, move |t| t.search(&q), |this, res, _, _| match res {
            Ok(h) => {
                this.set_msg(format!("{} matches", h.len()));
                this.hits = h;
            }
            Err(e) => this.set_msg(format!("error: search: {e}")),
        });
    }

    /// Every 3s: stat open files + refresh git in one background job. Skipped while one is in flight.
    fn tick(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.polling {
            return;
        }
        self.polling = true;
        let keys: Vec<String> = self.tabs.iter().filter(|t| !t.readonly).map(|t| t.key.clone()).collect();
        let (t, g) = (self.target.clone(), self.gen_);
        cx.spawn_in(window, async move |this, cx| {
            let r = cx
                .background_spawn(async move {
                    let m = if keys.is_empty() { Ok(vec![]) } else { t.mtimes(&keys) };
                    (keys, m, t.git_branch().map(|b| (b, t.git_status())))
                })
                .await;
            this.update_in(cx, |this, w, cx| {
                this.polling = false;
                if this.gen_ == g {
                    this.apply_poll(r, w, cx);
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    fn apply_poll(
        &mut self,
        (keys, mtimes, git): (Vec<String>, Result<Vec<Option<i64>>, String>, Option<(String, Result<Vec<(String, String)>, String>)>),
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match git {
            None => (self.branch, self.status) = (None, vec![]),
            Some((b, st)) => {
                self.branch = Some(b);
                if let Ok(s) = st {
                    self.status = s;
                }
            }
        }
        let Ok(ms) = mtimes else { return };
        for (key, m) in keys.into_iter().zip(ms) {
            let Some(tab) = self.tabs.iter_mut().find(|t| t.key == key) else { continue };
            let Some(m) = m else { continue };
            if tab.mtime.replace(m).is_none_or(|o| o == m) {
                continue;
            }
            if tab.dirty {
                self.set_msg(format!("{key} changed on disk"));
            } else {
                self.reload_tab(key, window, cx);
            }
        }
    }

    fn reload_tab(&mut self, key: String, window: &mut Window, cx: &mut Context<Self>) {
        let k = key.clone();
        self.bg(window, cx, move |t| t.read(&k), move |this, res, w, cx| {
            let Ok(text) = res else { return };
            let Some(tab) = this.tabs.iter_mut().find(|t| t.key == key) else { return };
            if tab.dirty || tab.saved == text {
                return;
            }
            let pos = tab.state.read(cx).cursor_position();
            tab.saved = text.clone();
            tab.state.update(cx, |s, cx| {
                s.set_value(text, w, cx);
                s.set_cursor_position(pos, w, cx);
            });
            this.set_msg(format!("{key} reloaded"));
        });
    }

    fn forward(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let v = self.port_input.read(cx).value().to_string();
        let (l, r) = v.trim().split_once(':').unwrap_or((v.trim(), v.trim()));
        let (Ok(l), Ok(r)) = (l.trim().parse::<u16>(), r.trim().parse::<u16>()) else {
            return self.set_msg("error: ports must be `port` or `local:remote`");
        };
        self.bg(window, cx, move |t| t.forward(l, r), move |this, res, _, _| match res {
            Ok(f) => {
                this.set_msg(format!("forwarding localhost:{l} -> remote:{r}"));
                this.forwards.push(f);
            }
            Err(e) => this.set_msg(format!("error: forward {l}:{r}: {e}")),
        });
    }

    fn stop_forwards(&mut self, cx: &mut Context<Self>) {
        let fs = std::mem::take(&mut self.forwards);
        if !fs.is_empty() {
            cx.background_spawn(async move { fs.into_iter().for_each(Forward::stop) }).detach();
        }
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

/// Row height shared by every list (tree, search hits, changes) so the panels line up.
const ROW_H: Pixels = px(24.0);
/// Height of panel header strips (sidebar, tab bar, bottom panel) so their bottom borders align.
const HEADER_H: Pixels = px(34.0);

fn file_name(path: &str) -> &str {
    path.rsplit('/').next().unwrap_or(path)
}

fn dir_name(path: &str) -> &str {
    path.rsplit_once('/').map_or("", |(d, _)| d)
}

impl Render for Redit {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let th = cx.theme().clone();
        let (border, muted, hover) = (th.border, th.muted_foreground, th.sidebar_accent);
        let icon = |name: IconName, color: Hsla| Icon::new(name).size_3p5().flex_shrink_0().text_color(color);
        let header = |label: &'static str| {
            div()
                .h(HEADER_H)
                .flex_shrink_0()
                .flex()
                .items_center()
                .gap_1()
                .px_3()
                .text_xs()
                .font_semibold()
                .text_color(muted)
                .child(div().flex_1().child(label))
        };
        let active_key = self.tabs.get(self.active).map(|t| t.key.clone());

        let mut rows = vec![];
        self.rows("", 0, &mut rows);
        let tree = div().id("tree").flex_1().min_h_0().overflow_y_scroll().pb_2().children(
            rows.into_iter().enumerate().map(|(i, (path, is_dir, depth, open))| {
                let current = active_key.as_deref() == Some(path.as_str());
                let chevron = if open { IconName::ChevronDown } else { IconName::ChevronRight };
                let kind = match (is_dir, open) {
                    (true, true) => IconName::FolderOpen,
                    (true, false) => IconName::Folder,
                    _ => IconName::File,
                };
                div()
                    .id(("row", i))
                    .h(ROW_H)
                    .flex()
                    .items_center()
                    .gap_1p5()
                    .pl(px(10.0 + 14.0 * depth as f32))
                    .pr_2()
                    .text_sm()
                    .cursor_pointer()
                    .hover(move |s| s.bg(hover))
                    .when(current, |d| d.bg(th.sidebar_accent).text_color(th.sidebar_accent_foreground))
                    .child(if is_dir { icon(chevron, muted).into_any_element() } else { div().w(px(14.0)).into_any_element() })
                    .child(icon(kind, muted))
                    .child(div().truncate().child(file_name(&path).to_string()))
                    .on_click(cx.listener(move |this, _, w, cx| {
                        if is_dir {
                            this.click_dir(path.clone(), w, cx)
                        } else {
                            this.open_file(path.clone(), None, w, cx)
                        }
                    }))
            }),
        );

        let mut prev = "";
        let mut hit_rows = vec![];
        for (i, h) in self.hits.iter().enumerate() {
            if h.path != prev {
                prev = &h.path;
                hit_rows.push(
                    div()
                        .id(("hf", i))
                        .h(ROW_H)
                        .mt_1()
                        .flex()
                        .items_center()
                        .gap_1p5()
                        .px_3()
                        .text_sm()
                        .child(icon(IconName::File, muted))
                        .child(div().flex_shrink_0().font_medium().child(file_name(&h.path).to_string()))
                        .child(div().truncate().text_xs().text_color(muted).child(dir_name(&h.path).to_string())),
                );
            }
            let txt: String = h.text.trim().chars().take(200).collect();
            let (p, l) = (h.path.clone(), h.line);
            hit_rows.push(
                div()
                    .id(("hit", i))
                    .h(ROW_H)
                    .flex()
                    .items_center()
                    .gap_2()
                    .pl(px(32.0))
                    .pr_3()
                    .text_xs()
                    .cursor_pointer()
                    .hover(move |s| s.bg(hover))
                    .child(div().w(px(28.0)).flex_shrink_0().text_right().text_color(muted).child(l.to_string()))
                    .child(div().truncate().font_family(th.mono_font_family.clone()).child(txt))
                    .on_click(cx.listener(move |this, _, w, cx| this.open_file(p.clone(), Some(l.saturating_sub(1)), w, cx))),
            );
        }
        let search_empty = self.hits.is_empty().then(|| {
            div().px_3().py_2().text_xs().text_color(muted).child("Press Enter to search the workspace.")
        });
        let search = div()
            .flex_1()
            .min_h_0()
            .flex()
            .flex_col()
            .child(div().px_2().pb_2().child(Input::new(&self.search_input).small().prefix(icon(IconName::Search, muted))))
            .children(search_empty)
            .child(div().id("hits").flex_1().min_h_0().overflow_y_scroll().pb_2().children(hit_rows));

        let mode = self.search_mode;
        let mode_btn = |id: &'static str, name: IconName, tip: &'static str, on: bool, m: bool| {
            Button::new(id).ghost().xsmall().icon(name).selected(on).tooltip(tip).on_click(cx.listener(move |this, _, _, cx| {
                this.search_mode = m;
                cx.notify();
            }))
        };
        let sidebar = div()
            .w(px(248.0))
            .flex_shrink_0()
            .flex()
            .flex_col()
            .bg(th.sidebar)
            .text_color(th.sidebar_foreground)
            .border_r_1()
            .border_color(th.sidebar_border)
            .child(
                header(if mode { "Search" } else { "Explorer" })
                    .child(mode_btn("m-files", IconName::Files, "Explorer", !mode, false))
                    .child(mode_btn("m-search", IconName::Search, "Search in files", mode, true)),
            )
            .child(if mode { search.into_any_element() } else { tree.into_any_element() });

        let tabs = div()
            .id("tabs")
            .h(HEADER_H)
            .flex_shrink_0()
            .flex()
            .flex_row()
            .bg(th.tab_bar)
            .border_b_1()
            .border_color(border)
            .overflow_x_scroll()
            .children(self.tabs.iter().enumerate().map(|(i, t)| {
                let act = i == self.active;
                let group: SharedString = format!("tab{i}").into();
                div()
                    .id(("tab", i))
                    .group(group.clone())
                    .h_full()
                    .flex_shrink_0()
                    .flex()
                    .items_center()
                    .gap_1p5()
                    .pl_3()
                    .pr_1p5()
                    .text_sm()
                    .cursor_pointer()
                    .border_r_1()
                    .border_color(border)
                    .map(|d| if act {
                        // Active tab merges into the editor: same bg, and it covers the bar's bottom border.
                        d.bg(th.background).text_color(th.foreground).mb(px(-1.0))
                    } else {
                        d.text_color(muted).hover(|s| s.text_color(th.foreground))
                    })
                    .child(icon(IconName::File, muted))
                    .child(t.title.clone())
                    .child(
                        div()
                            .id(("close", i))
                            .size_5()
                            .flex()
                            .items_center()
                            .justify_center()
                            .rounded_sm()
                            .hover(|s| s.bg(th.accent))
                            .map(|d| if t.dirty {
                                // Dirty: show a dot; it turns into the close button on hover.
                                d.child(div().size_2().rounded_full().bg(th.foreground).group_hover(group.clone(), |s| s.invisible()))
                                    .child(div().absolute().invisible().group_hover(group.clone(), |s| s.visible()).child(icon(IconName::Close, muted)))
                            } else {
                                d.child(div().when(!act, |d| d.invisible().group_hover(group.clone(), |s| s.visible())).child(icon(IconName::Close, muted)))
                            })
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.tabs.remove(i);
                                this.active = this.active.min(this.tabs.len().saturating_sub(1));
                                cx.stop_propagation();
                                cx.notify();
                            })),
                    )
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.active = i;
                        cx.notify();
                    }))
            }));

        let shortcut = |label: &'static str, action: &dyn Action| {
            div()
                .flex()
                .items_center()
                .justify_between()
                .gap_8()
                .child(div().text_sm().text_color(muted).child(label))
                .children(Kbd::binding_for_action(action, None, window))
        };
        let editor = match self.tabs.get(self.active) {
            Some(t) => div().flex_1().min_h_0().child(Editor::new(&t.state).appearance(false).h_full()),
            None => div().flex_1().flex().items_center().justify_center().child(
                div()
                    .w(px(260.0))
                    .flex()
                    .flex_col()
                    .gap_3()
                    .child(shortcut("Search in files", &FocusSearch))
                    .child(shortcut("Toggle terminal", &ToggleBottom))
                    .child(shortcut("Save file", &Save)),
            ),
        };

        let git_btn = |id: &'static str, name: IconName, tip: &'static str| Button::new(id).ghost().xsmall().icon(name).tooltip(tip);
        let git = match self.branch.clone() {
            None => div()
                .flex_1()
                .flex()
                .flex_col()
                .child(header("Source control"))
                .child(div().px_3().text_xs().text_color(muted).child("This folder is not a git repository.")),
            Some(b) => div()
                .flex_1()
                .min_h_0()
                .flex()
                .flex_col()
                .child(
                    header("Source control")
                        .child(git_btn("refresh", IconName::RefreshCw, "Refresh").on_click(cx.listener(|this, _, w, cx| this.refresh_git(w, cx))))
                        .child(git_btn("pull", IconName::ArrowDown, "Pull").on_click(cx.listener(|this, _, w, cx| this.git_op("pull", w, cx, |t| t.git_pull()))))
                        .child(git_btn("push", IconName::ArrowUp, "Push").on_click(cx.listener(|this, _, w, cx| this.git_op("push", w, cx, |t| t.git_push())))),
                )
                .child(
                    div()
                        .flex()
                        .flex_col()
                        .gap_2()
                        .px_3()
                        .pb_3()
                        .child(div().flex().items_center().gap_1p5().text_sm().child(icon(IconName::GitBranch, muted)).child(div().truncate().child(b)))
                        .child(Input::new(&self.commit_input).small())
                        .child(
                            Button::new("commit")
                                .small()
                                .primary()
                                .w_full()
                                .label(match self.status.len() {
                                    0 => "Commit".to_string(),
                                    1 => "Commit 1 change".to_string(),
                                    n => format!("Commit {n} changes"),
                                })
                                .disabled(self.status.is_empty())
                                .on_click(cx.listener(|this, _, w, cx| this.commit(w, cx))),
                        ),
                )
                .child(header("Changes").border_t_1().border_color(th.sidebar_border))
                .when(self.status.is_empty(), |d| {
                    d.child(div().px_3().text_xs().text_color(muted).child("Working tree clean."))
                })
                .child(
                    div().id("changes").flex_1().min_h_0().overflow_y_scroll().pb_2().children(
                        self.status.iter().enumerate().map(|(i, (xy, p))| {
                            let (p2, untracked) = (p.clone(), xy == "??");
                            let (letter, color) = match xy.trim().chars().next() {
                                Some('?') => ("U", th.success),
                                Some('A') => ("A", th.success),
                                Some('D') => ("D", th.danger),
                                Some('R') => ("R", th.info),
                                _ => ("M", th.warning),
                            };
                            div()
                                .id(("chg", i))
                                .h(ROW_H)
                                .flex()
                                .items_center()
                                .gap_1p5()
                                .px_3()
                                .text_sm()
                                .cursor_pointer()
                                .hover(move |s| s.bg(hover))
                                .child(icon(IconName::File, muted))
                                .child(div().flex_shrink_0().child(file_name(p).to_string()))
                                .child(div().flex_1().truncate().text_xs().text_color(muted).child(dir_name(p).to_string()))
                                .child(div().w_3().text_xs().font_semibold().text_color(color).child(letter))
                                .on_click(cx.listener(move |this, _, w, cx| {
                                    if untracked {
                                        this.open_file(p2.clone(), None, w, cx)
                                    } else {
                                        this.open_diff(p2.clone(), w, cx)
                                    }
                                }))
                        }),
                    ),
                ),
        };

        let ports = matches!(self.target, Target::Ssh { .. }).then(|| {
            div()
                .flex()
                .flex_col()
                .pb_3()
                .border_t_1()
                .border_color(th.sidebar_border)
                .child(header("Ports"))
                .child(
                    div()
                        .flex()
                        .gap_1()
                        .px_3()
                        .child(div().flex_1().child(Input::new(&self.port_input).small()))
                        .child(Button::new("fwd").small().outline().label("Forward").on_click(cx.listener(|this, _, w, cx| this.forward(w, cx)))),
                )
                .children(self.forwards.iter().enumerate().map(|(i, f)| {
                    div()
                        .h(ROW_H)
                        .mt_1()
                        .flex()
                        .items_center()
                        .gap_1p5()
                        .px_3()
                        .text_sm()
                        .child(icon(IconName::Network, muted))
                        .child(div().flex_1().truncate().child(format!("localhost:{} → {}", f.local, f.remote)))
                        .child(Button::new(("stop", i)).ghost().xsmall().icon(IconName::Close).tooltip("Stop forwarding").on_click(cx.listener(move |this, _, _, cx| {
                            if i < this.forwards.len() {
                                let f = this.forwards.remove(i);
                                cx.background_spawn(async move { f.stop() }).detach();
                            }
                            cx.notify();
                        })))
                }))
        });
        let right = div()
            .w(px(280.0))
            .flex_shrink_0()
            .flex()
            .flex_col()
            .bg(th.sidebar)
            .text_color(th.sidebar_foreground)
            .border_l_1()
            .border_color(th.sidebar_border)
            .child(git)
            .children(ports);

        let remote = matches!(self.target, Target::Ssh { .. });
        let title_bar = TitleBar::new().child(
            div()
                .flex_1()
                .flex()
                .items_center()
                .justify_center()
                .gap_1p5()
                .pr_2()
                .child(div().w(px(180.0)).child(Select::new(&self.host_select).small().placeholder("SSH host")))
                .child(div().w(px(420.0)).child(Input::new(&self.target_input).small().prefix(icon(if remote { IconName::Globe } else { IconName::Folder }, muted))))
                .child(Button::new("open").small().outline().label("Open").on_click(cx.listener(|this, _, w, cx| this.open_target(w, cx)))),
        );

        let status_bar = div()
            .h(px(24.0))
            .flex_shrink_0()
            .flex()
            .items_center()
            .gap_4()
            .px_3()
            .text_xs()
            .text_color(muted)
            .bg(th.status_bar)
            .border_t_1()
            .border_color(th.status_bar_border)
            .child(div().flex().items_center().gap_1().child(icon(if remote { IconName::Globe } else { IconName::HardDrive }, muted)).child(match &self.target {
                // "." reads as nothing in the status bar; show the folder's real name.
                Target::Local { root } => std::fs::canonicalize(root)
                    .ok()
                    .and_then(|p| p.file_name().map(|n| n.to_string_lossy().into_owned()))
                    .unwrap_or_else(|| root.clone()),
                t => t.label(),
            }))
            .children(self.branch.clone().map(|b| div().flex().items_center().gap_1().child(icon(IconName::GitBranch, muted)).child(b)))
            .child(div().flex_1().truncate().child(self.msg.clone()))
            .when(self.busy > 0, |d| {
                d.child(
                    div()
                        .flex()
                        .items_center()
                        .gap_1()
                        .child(Spinner::new().xsmall().color(muted))
                        .child(if remote { "Connecting…" } else { "Working…" }),
                )
            })
            .child(
                Button::new("term")
                    .ghost()
                    .xsmall()
                    .icon(IconName::SquareTerminal)
                    .selected(self.bottom_open)
                    .tooltip("Toggle terminal")
                    .on_click(|_, w, cx| w.dispatch_action(ToggleBottom.boxed_clone(), cx)),
            );

        div()
            .id("root")
            .key_context("Redit")
            .track_focus(&self.focus)
            .on_action(cx.listener(|this, _: &Save, w, cx| this.save(w, cx)))
            .on_action(cx.listener(|this, _: &FocusSearch, w, cx| {
                this.search_mode = true;
                this.search_input.update(cx, |s, cx| s.focus(w, cx));
                cx.notify();
            }))
            .on_action(cx.listener(|this, _: &ToggleBottom, w, cx| {
                this.bottom_open = !this.bottom_open;
                if this.bottom_open && this.bottom.is_none() {
                    // shell() may wait on the ssh master: build the command off the UI thread.
                    this.bg(w, cx, |t| t.shell(), |this, cmd, w, cx| {
                        this.bottom = Some(cx.new(|cx| terminal::TerminalView::new(cmd, w, cx)).into());
                    });
                }
                cx.notify();
            }))
            .size_full()
            .flex()
            .flex_col()
            .bg(th.background)
            .text_color(th.foreground)
            .child(title_bar)
            .child(
                div()
                    .flex_1()
                    .min_h_0()
                    .flex()
                    .flex_row()
                    .child(sidebar)
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .flex()
                            .flex_col()
                            .when(!self.tabs.is_empty(), |d| d.child(tabs))
                            .child(editor)
                            .when(self.bottom_open, |d| {
                                d.child(
                                    div()
                                        .h(relative(0.35))
                                        .flex()
                                        .flex_col()
                                        .border_t_1()
                                        .border_color(border)
                                        .child(
                                            header("Terminal").child(
                                                git_btn("close-term", IconName::Close, "Close terminal")
                                                    .on_click(|_, w, cx| w.dispatch_action(ToggleBottom.boxed_clone(), cx)),
                                            ),
                                        )
                                        .child(div().flex_1().min_h_0().px_2().children(self.bottom.clone())),
                                )
                            }),
                    )
                    .child(right),
            )
            .child(status_bar)
    }
}

fn main() {
    if std::env::var_os(target::ASKPASS_ENV).is_some() {
        return panels::run_askpass(std::env::args().nth(1).unwrap_or_default());
    }
    let arg = std::env::args().nth(1).unwrap_or_else(|| ".".into());
    gpui_kit::application().with_assets(gpui_kit::assets::AllAssets).run(move |cx| {
        gpui_kit::init(cx);
        // gpui bindings have no `secondary-`: cmd on mac, ctrl elsewhere
        let [save, find, term] = if cfg!(target_os = "macos") { ["cmd-s", "cmd-shift-f", "cmd-j"] } else { ["ctrl-s", "ctrl-shift-f", "ctrl-j"] };
        cx.bind_keys([
            KeyBinding::new(save, Save, None),
            KeyBinding::new(find, FocusSearch, None),
            // Kbd hints show the last binding for an action, so the primary one goes last.
            KeyBinding::new("ctrl-`", ToggleBottom, None),
            KeyBinding::new(term, ToggleBottom, None),
        ]);
        cx.on_window_closed(|cx, _| {
            if cx.windows().is_empty() {
                cx.quit();
            }
        })
        .detach();
        cx.activate(true);
        let a = arg.clone();
        let opts = WindowOptions {
            window_min_size: Some(size(px(900.0), px(560.0))),
            ..TitleBar::window_options()
        };
        gpui_kit::open_window(opts, cx, move |window, cx| {
            Theme::sync_system_appearance(Some(window), cx);
            let v = cx.new(|cx| Redit::new(a, window, cx));
            let f = v.read(cx).focus.clone();
            f.focus(window, cx);
            v
        })
        .expect("failed to open window");
    });
}
