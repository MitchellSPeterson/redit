//! Minimal LSP client: one server per server binary per workspace, started lazily,
//! JSON-RPC over the stdio of `Target::process` (so remote servers tunnel through ssh).
//! All blocking IO lives on plain threads / gpui background tasks, never the UI thread.
//! Features: diagnostics, completion, hover, go-to-definition. No rename/format/actions.
//!
//! Wiring: after creating an `EditorState`, call
//! `lsp::attach(&state, &lsp_handle, rel_path, open_cb, err_cb, cx)`; after saving a file
//! call `lsp_handle.did_save(rel)` (from a background thread).
//! ponytail: positions use gpui's char columns (== UTF-16 for BMP text); astral chars drift.

use crate::target::Target;
use anyhow::anyhow;
use gpui_kit::component::input::{
    CompletionProvider, DefinitionProvider, EditorState, HoverProvider, InputEvent, Rope, RopeExt,
    ShowDocumentHandler,
};
use gpui_kit::*;
use lsp_types::{
    CompletionContext, CompletionItem, CompletionResponse, Diagnostic, GotoDefinitionResponse, Hover,
    LocationLink, Position, Range, Uri, request::Request,
};
use serde::Serialize;
use serde_json::{Value, json};
use std::collections::HashMap;
use std::io::{BufRead, BufReader, Read as _, Write};
use std::process::{Child, Stdio};
use std::rc::Rc;
use std::str::FromStr;
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::mpsc::{Receiver, Sender, channel};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

/// (language id, server argv) by file extension.
fn language(rel: &str) -> Option<(&'static str, &'static [&'static str])> {
    const RA: &[&str] = &["rust-analyzer"];
    const CLANGD: &[&str] = &["clangd"];
    const TS: &[&str] = &["typescript-language-server", "--stdio"];
    const PY: &[&str] = &["pyright-langserver", "--stdio"];
    const GO: &[&str] = &["gopls"];
    Some(match rel.rsplit_once('.')?.1 {
        "rs" => ("rust", RA),
        "c" | "h" => ("c", CLANGD),
        "cc" | "cpp" | "cxx" | "hpp" | "hh" => ("cpp", CLANGD),
        "ts" => ("typescript", TS),
        "tsx" => ("typescriptreact", TS),
        "js" => ("javascript", TS),
        "jsx" => ("javascriptreact", TS),
        "py" => ("python", PY),
        "go" => ("go", GO),
        _ => return None,
    })
}

// ---------- URIs ----------

fn path_to_uri(abs: &str) -> String {
    let mut s = String::from("file://");
    for b in abs.bytes() {
        if b.is_ascii_alphanumeric() || b"/-._~".contains(&b) {
            s.push(b as char);
        } else {
            s.push_str(&format!("%{b:02X}"));
        }
    }
    s
}

fn uri_to_path(uri: &str) -> Option<String> {
    let p = uri.strip_prefix("file://")?.as_bytes();
    let mut out = Vec::with_capacity(p.len());
    let mut i = 0;
    while i < p.len() {
        if p[i] == b'%' && i + 2 < p.len() {
            out.push(u8::from_str_radix(std::str::from_utf8(&p[i + 1..i + 3]).ok()?, 16).ok()?);
            i += 3;
        } else {
            out.push(p[i]);
            i += 1;
        }
    }
    String::from_utf8(out).ok()
}

fn uri(s: &str) -> Result<Uri, String> {
    Uri::from_str(s).map_err(|e| e.to_string())
}

// ---------- client ----------

type Pending = Arc<Mutex<HashMap<i64, Sender<Result<Value, String>>>>>;
type Diags = Arc<Mutex<HashMap<String, (u64, Vec<Diagnostic>)>>>;

struct Client {
    tx: Sender<Vec<u8>>,
    next_id: AtomicI64,
    pending: Pending,
    /// uri -> (generation, diagnostics)
    diags: Diags,
    /// uri -> (version, text)
    docs: Mutex<HashMap<String, (i32, String)>>,
    child: Mutex<Child>,
}

fn frame(v: &Value) -> Vec<u8> {
    let body = v.to_string();
    format!("Content-Length: {}\r\n\r\n{body}", body.len()).into_bytes()
}

fn read_msg(r: &mut impl BufRead) -> Option<Value> {
    let mut len = None;
    loop {
        let mut line = String::new();
        if r.read_line(&mut line).ok()? == 0 {
            return None;
        }
        let line = line.trim();
        if line.is_empty() {
            break;
        }
        if let Some(v) = line.to_ascii_lowercase().strip_prefix("content-length:") {
            len = v.trim().parse::<usize>().ok();
        }
    }
    let mut buf = vec![0; len?];
    r.read_exact(&mut buf).ok()?;
    serde_json::from_slice(&buf).ok()
}

impl Client {
    fn start(argv: &[&str], target: &Target, root: &str) -> Result<Arc<Client>, String> {
        let mut cmd = target.process(argv);
        let mut child = cmd
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .map_err(|e| format!("cannot start language server `{}`: {e}", argv[0]))?;
        let mut stdin = child.stdin.take().unwrap();
        let stdout = child.stdout.take().unwrap();
        let (tx, rx): (Sender<Vec<u8>>, Receiver<Vec<u8>>) = channel();
        std::thread::spawn(move || {
            while let Ok(b) = rx.recv() {
                if stdin.write_all(&b).and_then(|_| stdin.flush()).is_err() {
                    break;
                }
            }
        });
        let pending: Pending = Default::default();
        let diags: Diags = Default::default();
        {
            let (tx, pending, diags) = (tx.clone(), pending.clone(), diags.clone());
            std::thread::spawn(move || {
                let mut r = BufReader::new(stdout);
                while let Some(m) = read_msg(&mut r) {
                    dispatch(m, &tx, &pending, &diags);
                }
                pending.lock().unwrap().clear(); // wakes waiters with a disconnect error
            });
        }
        let c = Arc::new(Client {
            tx,
            next_id: AtomicI64::new(1),
            pending,
            diags,
            docs: Default::default(),
            child: Mutex::new(child),
        });
        let root_uri = path_to_uri(root);
        let name = root.rsplit('/').next().unwrap_or("root");
        c.request_raw(
            "initialize",
            json!({
                "processId": std::process::id(),
                "clientInfo": {"name": "redit"},
                "rootUri": root_uri,
                "workspaceFolders": [{"uri": root_uri, "name": name}],
                "capabilities": {
                    "general": {"positionEncodings": ["utf-16"]},
                    "textDocument": {
                        "synchronization": {"didSave": true},
                        "completion": {"completionItem": {"documentationFormat": ["markdown", "plaintext"]}},
                        "hover": {"contentFormat": ["markdown", "plaintext"]},
                        "definition": {"linkSupport": true},
                        "publishDiagnostics": {}
                    }
                }
            }),
            Duration::from_secs(120),
        )
        .map_err(|e| format!("language server `{}` failed to initialize (installed?): {e}", argv[0]))?;
        c.notify("initialized", json!({}));
        Ok(c)
    }

    fn notify(&self, method: &str, params: Value) {
        let _ = self.tx.send(frame(&json!({"jsonrpc":"2.0","method":method,"params":params})));
    }

    fn request_raw(&self, method: &str, params: Value, timeout: Duration) -> Result<Value, String> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = channel();
        self.pending.lock().unwrap().insert(id, tx);
        let sent = self.tx.send(frame(&json!({"jsonrpc":"2.0","id":id,"method":method,"params":params})));
        let res = if sent.is_err() {
            Err("server not running".to_string())
        } else {
            rx.recv_timeout(timeout).map_err(|_| format!("{method}: timed out or server exited")).and_then(|r| r)
        };
        self.pending.lock().unwrap().remove(&id);
        res
    }

    fn call<R: Request>(&self, params: R::Params) -> Result<R::Result, String>
    where
        R::Params: Serialize,
    {
        let p = serde_json::to_value(params).map_err(|e| e.to_string())?;
        serde_json::from_value(self.request_raw(R::METHOD, p, REQUEST_TIMEOUT)?).map_err(|e| e.to_string())
    }

    /// Open the doc, or send a full-text didChange if the text differs. Idempotent.
    fn sync(&self, uri: &str, lang: &str, text: &str) {
        let mut docs = self.docs.lock().unwrap();
        match docs.get_mut(uri) {
            None => {
                docs.insert(uri.into(), (1, text.into()));
                self.notify(
                    "textDocument/didOpen",
                    json!({"textDocument": {"uri":uri,"languageId":lang,"version":1,"text":text}}),
                );
            }
            Some((ver, old)) if old != text => {
                *ver += 1;
                *old = text.into();
                self.notify(
                    "textDocument/didChange",
                    json!({"textDocument":{"uri":uri,"version":*ver},"contentChanges":[{"text":text}]}),
                );
            }
            _ => {}
        }
    }
}

fn dispatch(m: Value, tx: &Sender<Vec<u8>>, pending: &Pending, diags: &Diags) {
    let method = m.get("method").and_then(Value::as_str);
    match (method, m.get("id")) {
        // server -> client request: answer minimally so servers don't stall
        (Some(method), Some(id)) => {
            let result = match method {
                "workspace/configuration" => {
                    let n = m["params"]["items"].as_array().map_or(0, Vec::len);
                    Value::Array(vec![Value::Null; n])
                }
                _ => Value::Null, // workDoneProgress/create, client/registerCapability, ...
            };
            let _ = tx.send(frame(&json!({"jsonrpc":"2.0","id":id,"result":result})));
        }
        (Some("textDocument/publishDiagnostics"), None) => {
            if let Ok(p) = serde_json::from_value::<lsp_types::PublishDiagnosticsParams>(m["params"].clone()) {
                let mut d = diags.lock().unwrap();
                let e = d.entry(p.uri.to_string()).or_default();
                *e = (e.0 + 1, p.diagnostics);
            }
        }
        (Some(_), None) => {}
        (None, Some(id)) => {
            let Some(tx) = id.as_i64().and_then(|i| pending.lock().unwrap().remove(&i)) else {
                return;
            };
            let _ = tx.send(match m.get("error") {
                Some(e) => Err(e["message"].as_str().unwrap_or("lsp error").to_string()),
                None => Ok(m.get("result").cloned().unwrap_or(Value::Null)),
            });
        }
        _ => {}
    }
}

impl Drop for Client {
    fn drop(&mut self) {
        let _ = self.request_raw("shutdown", Value::Null, Duration::from_secs(2));
        self.notify("exit", Value::Null);
        std::thread::sleep(Duration::from_millis(100)); // let `exit` flush before the hard kill
        if let Ok(mut c) = self.child.lock() {
            let _ = c.kill();
            let _ = c.wait();
        }
    }
}

// ---------- handle ----------

type Cell = Arc<OnceLock<Result<Arc<Client>, String>>>;

struct Inner {
    target: Target,
    root: OnceLock<String>,
    /// keyed by server argv[0]; the OnceLock serializes concurrent startups
    servers: Mutex<HashMap<&'static str, Cell>>,
}

/// Cheap to clone; shared by all editors of a workspace.
/// Methods taking `rel` may block (server startup, requests): never call on the UI thread,
/// except `diagnostics`/`uri_to_rel` which never block once a server is up.
#[derive(Clone)]
pub struct LspHandle(Arc<Inner>);

impl LspHandle {
    pub fn new(target: Target) -> Self {
        Self(Arc::new(Inner { target, root: OnceLock::new(), servers: Default::default() }))
    }

    fn root(&self) -> Result<&str, String> {
        if self.0.root.get().is_none() {
            let _ = self.0.root.set(self.0.target.abs_root()?);
        }
        Ok(self.0.root.get().unwrap())
    }

    fn uri_of(&self, rel: &str) -> Result<String, String> {
        Ok(path_to_uri(&format!("{}/{rel}", self.root()?.trim_end_matches('/'))))
    }

    /// Server for this file, started (and initialized) on first use. Err if no server for the
    /// extension, binary missing, or init failed (the failure is cached).
    fn client(&self, rel: &str) -> Result<(Arc<Client>, &'static str), String> {
        let (lang, argv) = language(rel).ok_or_else(|| format!("no language server for `{rel}`"))?;
        let cell = self.0.servers.lock().unwrap().entry(argv[0]).or_default().clone();
        cell.get_or_init(|| Client::start(argv, &self.0.target, self.root()?)).clone().map(|c| (c, lang))
    }

    /// Open the file (starting the server if needed), or send didChange if the text changed.
    pub fn sync(&self, rel: &str, text: &str) -> Result<(), String> {
        let (c, lang) = self.client(rel)?;
        c.sync(&self.uri_of(rel)?, lang, text);
        Ok(())
    }

    pub fn did_save(&self, rel: &str) {
        if let (Ok((c, _)), Ok(u)) = (self.client(rel), self.uri_of(rel)) {
            c.notify("textDocument/didSave", json!({"textDocument":{"uri":u}}));
        }
    }

    pub fn did_close(&self, rel: &str) {
        if let (Ok((c, _)), Ok(u)) = (self.client(rel), self.uri_of(rel)) {
            if c.docs.lock().unwrap().remove(&u).is_some() {
                c.notify("textDocument/didClose", json!({"textDocument":{"uri":u}}));
            }
        }
    }

    /// Latest pushed diagnostics: (generation, list); generation bumps on each publish.
    /// Non-blocking: None until the server is up and has published for this file.
    pub fn diagnostics(&self, rel: &str) -> Option<(u64, Vec<Diagnostic>)> {
        let (_, argv) = language(rel)?;
        let cell = self.0.servers.lock().unwrap().get(argv[0])?.clone();
        let c = cell.get()?.as_ref().ok()?;
        let u = path_to_uri(&format!("{}/{rel}", self.0.root.get()?.trim_end_matches('/')));
        c.diags.lock().unwrap().get(&u).cloned()
    }

    pub fn completion(&self, rel: &str, pos: Position, ctx: Option<CompletionContext>) -> Result<Vec<CompletionItem>, String> {
        let (c, _) = self.client(rel)?;
        let r = c.call::<lsp_types::request::Completion>(lsp_types::CompletionParams {
            text_document_position: tdp(&self.uri_of(rel)?, pos)?,
            work_done_progress_params: Default::default(),
            partial_result_params: Default::default(),
            context: ctx,
        })?;
        Ok(match r {
            Some(CompletionResponse::Array(v)) => v,
            Some(CompletionResponse::List(l)) => l.items,
            None => vec![],
        })
    }

    pub fn hover(&self, rel: &str, pos: Position) -> Result<Option<Hover>, String> {
        let (c, _) = self.client(rel)?;
        c.call::<lsp_types::request::HoverRequest>(lsp_types::HoverParams {
            text_document_position_params: tdp(&self.uri_of(rel)?, pos)?,
            work_done_progress_params: Default::default(),
        })
    }

    /// Definition locations; `target_uri` is the server's (machine-absolute) URI, see `uri_to_rel`.
    pub fn definition(&self, rel: &str, pos: Position) -> Result<Vec<LocationLink>, String> {
        let (c, _) = self.client(rel)?;
        let r = c.call::<lsp_types::request::GotoDefinition>(lsp_types::GotoDefinitionParams {
            text_document_position_params: tdp(&self.uri_of(rel)?, pos)?,
            work_done_progress_params: Default::default(),
            partial_result_params: Default::default(),
        })?;
        Ok(match r {
            Some(GotoDefinitionResponse::Link(v)) => v,
            Some(GotoDefinitionResponse::Scalar(l)) => vec![loc_link(l)],
            Some(GotoDefinitionResponse::Array(v)) => v.into_iter().map(loc_link).collect(),
            None => vec![],
        })
    }

    /// Workspace-relative path for a server URI, or None if outside the workspace
    /// (or the root isn't resolved yet).
    pub fn uri_to_rel(&self, uri: &str) -> Option<String> {
        let root = self.0.root.get()?.trim_end_matches('/');
        Some(uri_to_path(uri)?.strip_prefix(root)?.strip_prefix('/')?.to_string())
    }
}

fn tdp(uri_s: &str, position: Position) -> Result<lsp_types::TextDocumentPositionParams, String> {
    Ok(lsp_types::TextDocumentPositionParams {
        text_document: lsp_types::TextDocumentIdentifier { uri: uri(uri_s)? },
        position,
    })
}

fn loc_link(l: lsp_types::Location) -> LocationLink {
    LocationLink {
        origin_selection_range: None,
        target_uri: l.uri,
        target_range: l.range,
        target_selection_range: l.range,
    }
}

// ---------- gpui adapter ----------

/// Called when Go to Definition lands in another workspace file: open `rel` and select `range`.
pub type OpenFn = Rc<dyn Fn(&str, Range, &mut Window, &mut App)>;

struct Provider {
    lsp: LspHandle,
    rel: String,
}

impl Provider {
    /// Run a blocking LSP call on a background thread, syncing the text first.
    fn run<T: Send + 'static>(
        &self,
        text: &Rope,
        offset: usize,
        cx: &mut App,
        f: impl FnOnce(&LspHandle, &str, Position) -> Result<T, String> + Send + 'static,
    ) -> Task<anyhow::Result<T>> {
        let (lsp, rel) = (self.lsp.clone(), self.rel.clone());
        let (snap, pos) = (text.to_string(), text.offset_to_position(offset));
        cx.background_spawn(async move {
            lsp.sync(&rel, &snap).map_err(|e| anyhow!(e))?;
            f(&lsp, &rel, pos).map_err(|e| anyhow!(e))
        })
    }
}

impl CompletionProvider for Provider {
    fn completions(
        &self,
        text: &Rope,
        offset: usize,
        trigger: CompletionContext,
        _: &mut Window,
        cx: &mut App,
    ) -> Task<anyhow::Result<CompletionResponse>> {
        self.run(text, offset, cx, move |l, rel, pos| {
            l.completion(rel, pos, Some(trigger)).map(CompletionResponse::Array)
        })
    }

    fn is_completion_trigger(&self, _: usize, new_text: &str, _: &mut App) -> bool {
        new_text.chars().last().is_some_and(|c| c.is_alphanumeric() || matches!(c, '_' | '.' | ':' | '>'))
    }
}

impl HoverProvider for Provider {
    fn hover(&self, text: &Rope, offset: usize, _: &mut Window, cx: &mut App) -> Task<anyhow::Result<Option<Hover>>> {
        self.run(text, offset, cx, |l, rel, pos| l.hover(rel, pos))
    }
}

impl DefinitionProvider for Provider {
    fn definitions(&self, text: &Rope, offset: usize, _: &mut Window, cx: &mut App) -> Task<anyhow::Result<Vec<LocationLink>>> {
        self.run(text, offset, cx, |l, rel, pos| l.definition(rel, pos))
    }
}

/// Hook `editor` up to the language server for `rel_path`.
/// Returns Err immediately if the extension has no known server. Server startup, didOpen,
/// didChange sync and diagnostics all happen in the background; a startup failure (binary
/// missing, init failed) is reported once via `on_error` (show it in the status bar).
/// `open` is called for definitions in other workspace files (same-file jumps are handled
/// by the editor itself). Dropping the editor closes the document.
pub fn attach(
    editor: &Entity<EditorState>,
    lsp: &LspHandle,
    rel_path: &str,
    open: OpenFn,
    on_error: impl Fn(String, &mut App) + 'static,
    cx: &mut App,
) -> Result<(), String> {
    language(rel_path).ok_or_else(|| format!("no language server for `{rel_path}`"))?;
    let rel = rel_path.to_string();
    let p = Rc::new(Provider { lsp: lsp.clone(), rel: rel.clone() });
    let show: ShowDocumentHandler = {
        let (lsp, rel) = (lsp.clone(), rel.clone());
        Rc::new(move |params, window, cx| match lsp.uri_to_rel(&params.uri.to_string()) {
            Some(r) if r == rel => false, // same file: editor jumps itself
            Some(r) => {
                open(&r, params.selection.unwrap_or_default(), window, cx);
                true
            }
            None => true, // outside workspace: ignore rather than jump in the wrong file
        })
    };
    editor.update(cx, |s, _| {
        let l = s.lsp_mut();
        l.completion_provider = Some(p.clone());
        l.hover_provider = Some(p.clone());
        l.definition_provider = Some(p);
        l.show_document = Some(show);
    });

    // Worker: start server + didOpen, then serialize didChange (latest text wins).
    let (tx, rx) = channel::<String>();
    let _ = tx.send(editor.read(cx).text().to_string());
    let (w_lsp, w_rel) = (lsp.clone(), rel.clone());
    let (err_tx, err_rx) = channel::<String>();
    std::thread::spawn(move || {
        while let Ok(mut text) = rx.recv() {
            while let Ok(t) = rx.try_recv() {
                text = t;
            }
            if let Err(e) = w_lsp.sync(&w_rel, &text) {
                let _ = err_tx.send(e);
                return;
            }
        }
        w_lsp.did_close(&w_rel); // editor dropped
    });
    cx.subscribe(editor, move |ed, ev: &InputEvent, cx| {
        if matches!(ev, InputEvent::Change) {
            let _ = tx.send(ed.read(cx).text().to_string());
        }
    })
    .detach();

    // Foreground poller: push diagnostics into the editor, surface a startup error.
    let (weak, lsp) = (editor.downgrade(), lsp.clone());
    cx.spawn(async move |cx| {
        let mut seen = 0;
        loop {
            cx.background_executor().timer(Duration::from_millis(300)).await;
            if let Ok(e) = err_rx.try_recv() {
                let _ = cx.update(|cx| on_error(e, cx));
                return;
            }
            let Some((generation, list)) = lsp.diagnostics(&rel) else { continue };
            if generation == seen {
                continue;
            }
            seen = generation;
            if weak
                .update(cx, |s, cx| {
                    let text = s.text().clone();
                    if let Some(d) = s.diagnostics_mut() {
                        d.reset(&text);
                        d.extend(list);
                    }
                    cx.notify();
                })
                .is_err()
            {
                return; // editor gone
            }
        }
    })
    .detach();
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use core::prelude::v1::test; // gpui_kit::* also exports a `test` macro

    fn tmp(tag: &str) -> std::path::PathBuf {
        let d = std::env::temp_dir().join(format!("redit-lsp-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn uri_roundtrip() {
        let u = path_to_uri("/a b/é#.c");
        assert_eq!(u, "file:///a%20b/%C3%A9%23.c");
        assert_eq!(uri_to_path(&u).unwrap(), "/a b/é#.c");
        assert!(language("x.tsx").is_some() && language("Makefile").is_none());
    }

    const SRC: &str = "int helper(int x) { return x + 1; }\nint main(void) {\n  int r = helper(2);\n  int u = undefined_name;\n  return r + u;\n}\n";

    /// Shared body: runs against any Target whose root contains `a.c` (written here).
    fn exercise(target: Target) {
        target.write("a.c", SRC).unwrap();
        let lsp = LspHandle::new(target);
        lsp.sync("a.c", SRC).expect("start clangd");

        let t0 = std::time::Instant::now();
        let diags = loop {
            if let Some((_, d)) = lsp.diagnostics("a.c").filter(|(_, d)| !d.is_empty()) {
                break d;
            }
            assert!(t0.elapsed() < Duration::from_secs(60), "no diagnostics");
            std::thread::sleep(Duration::from_millis(200));
        };
        assert!(diags.iter().any(|d| d.range.start.line == 3), "{diags:?}");

        // completion after `hel` on line 2
        let items = lsp.completion("a.c", Position::new(2, 13), None).unwrap();
        assert!(items.iter().any(|i| i.label.contains("helper")), "{} items", items.len());

        // definition of `helper` call (line 2, col 12) -> line 0 of a.c
        let defs = lsp.definition("a.c", Position::new(2, 12)).unwrap();
        assert_eq!(defs.len(), 1, "{defs:?}");
        assert_eq!(lsp.uri_to_rel(&defs[0].target_uri.to_string()).as_deref(), Some("a.c"));
        assert_eq!(defs[0].target_selection_range.start.line, 0);

        assert!(lsp.hover("a.c", Position::new(2, 12)).unwrap().is_some());
        drop(lsp); // shutdown/exit path must not hang
    }

    fn have(bin: &str) -> bool {
        std::process::Command::new(bin).arg("--version").stdout(Stdio::null()).stderr(Stdio::null()).status().is_ok()
    }

    #[test]
    fn clangd_local() {
        if !have("clangd") {
            return eprintln!("skip: clangd not installed");
        }
        let dir = tmp("local");
        exercise(Target::Local { root: dir.to_string_lossy().into() });
        std::fs::remove_dir_all(dir).unwrap();
    }

    /// Needs a throwaway sshd (SSH_AUTH_SOCK set) and clangd on the remote.
    #[test]
    #[ignore = "needs sshd at 127.0.0.1:2222"]
    fn clangd_ssh() {
        let dir = tmp("ssh");
        let host = std::env::var("REDIT_TEST_SSH").unwrap_or("ssh://mitchell@127.0.0.1:2222".into());
        exercise(Target::Ssh { host, root: dir.to_string_lossy().into() });
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    #[ignore = "slow: needs rust-analyzer (cargo test -- --ignored)"]
    fn rust_analyzer_local() {
        let dir = tmp("ra");
        std::fs::create_dir_all(dir.join("src")).unwrap();
        std::fs::write(dir.join("Cargo.toml"), "[package]\nname=\"t\"\nversion=\"0.1.0\"\nedition=\"2021\"\n").unwrap();
        let src = "fn helper() -> i32 { 1 }\nfn main() {\n    let r: i32 = helper();\n    let s: u8 = \"x\";\n}\n";
        std::fs::write(dir.join("src/main.rs"), src).unwrap();
        let lsp = LspHandle::new(Target::Local { root: dir.to_string_lossy().into() });
        lsp.sync("src/main.rs", src).unwrap();
        let t0 = std::time::Instant::now();
        loop {
            if lsp.diagnostics("src/main.rs").is_some_and(|(_, d)| !d.is_empty()) {
                break;
            }
            assert!(t0.elapsed() < Duration::from_secs(90), "no diagnostics");
            std::thread::sleep(Duration::from_millis(300));
        }
        let defs = lsp.definition("src/main.rs", Position::new(2, 18)).unwrap();
        assert_eq!(defs[0].target_selection_range.start.line, 0);
        std::fs::remove_dir_all(dir).unwrap();
    }
}
