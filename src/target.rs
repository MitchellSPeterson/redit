//! Where files live: local disk or a remote host over the system `ssh`.
//! Local: std::fs + `git` directly (portable, works on Windows).
//! Remote: POSIX sh scripts over ssh. Using system ssh = user's ~/.ssh/config, agent,
//! ProxyJump, keys all work for free; on Unix ControlMaster gives one multiplexed,
//! kept-alive connection so each op costs ~1 RTT, not a handshake.

use std::io::Write;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::time::Duration;

#[derive(Clone, Debug, PartialEq)]
pub enum Target {
    Local { root: String },
    Ssh { host: String, root: String },
}

#[derive(Clone, Debug, PartialEq)]
pub struct Entry {
    pub name: String,
    pub is_dir: bool,
}

/// Set on ssh child processes; when redit starts with it, it is acting as SSH_ASKPASS:
/// show argv[1] as a prompt, print the answer to stdout, exit 0 (or exit 1 on cancel).
pub const ASKPASS_ENV: &str = "REDIT_ASKPASS";

#[derive(Clone, Debug, PartialEq)]
pub struct SearchHit {
    pub path: String,
    pub line: u32,
    pub text: String,
}

const SKIP_DIRS: [&str; 4] = [".git", "node_modules", "target", ".venv"];
const MAX_HITS: usize = 2000;

/// `Host` aliases from ~/.ssh/config (wildcard patterns skipped), for a host picker.
// ponytail: ignores `Include` and Match blocks.
pub fn ssh_hosts() -> Vec<String> {
    let home = std::env::var_os("HOME").or_else(|| std::env::var_os("USERPROFILE"));
    let Some(cfg) = home.and_then(|h| std::fs::read_to_string(PathBuf::from(h).join(".ssh/config")).ok()) else {
        return vec![];
    };
    let mut v: Vec<String> = cfg
        .lines()
        .filter_map(|l| {
            let l = l.trim();
            let (k, rest) = l.split_once(char::is_whitespace)?;
            k.eq_ignore_ascii_case("host").then_some(rest)
        })
        .flat_map(|rest| rest.split_whitespace())
        .filter(|h| !h.contains(['*', '?', '!']))
        .map(String::from)
        .collect();
    v.dedup();
    v
}

/// Single-quote for POSIX sh. Paths come from the user / remote listings: never trust them.
pub fn sh_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', r"'\''"))
}

/// Active port forward; `stop()` removes it.
pub struct Forward {
    pub local: u16,
    pub remote: u16,
    host: String,
    spec: String,
    #[cfg(not(unix))]
    child: std::process::Child,
}

impl Forward {
    pub fn stop(#[allow(unused_mut)] mut self) {
        #[cfg(unix)]
        let _ = Target::ssh_base()
            .args(["-O", "cancel", "-L", &self.spec, &self.host])
            .stderr(Stdio::null())
            .status();
        #[cfg(not(unix))]
        let _ = (self.child.kill(), &self.host, &self.spec);
    }
}

fn search_dir(dir: &std::path::Path, rel: &str, q: &str, hits: &mut Vec<SearchHit>) {
    let Ok(rd) = std::fs::read_dir(dir) else { return };
    for e in rd.flatten() {
        if hits.len() >= MAX_HITS {
            return;
        }
        let name = e.file_name().to_string_lossy().into_owned();
        let rel = if rel.is_empty() { name.clone() } else { format!("{rel}/{name}") };
        let Ok(ft) = e.file_type() else { continue };
        if ft.is_dir() {
            if !SKIP_DIRS.contains(&name.as_str()) {
                search_dir(&e.path(), &rel, q, hits);
            }
        } else if ft.is_file() {
            let Ok(text) = std::fs::read_to_string(e.path()) else { continue }; // binary/non-UTF-8: skip
            if text.contains('\0') {
                continue;
            }
            for (i, l) in text.lines().enumerate() {
                if l.contains(q) {
                    hits.push(SearchHit { path: rel.clone(), line: i as u32 + 1, text: l.into() });
                    if hits.len() >= MAX_HITS {
                        return;
                    }
                }
            }
        }
    }
}

fn sort_entries(v: &mut [Entry]) {
    v.sort_by(|a, b| b.is_dir.cmp(&a.is_dir).then(a.name.cmp(&b.name)));
}

impl Target {
    /// `user@host:/path` → Ssh, anything else → Local.
    pub fn parse(s: &str) -> Target {
        // Windows drive letters (`C:\x`, `C:/x`) are local paths, not hosts.
        let is_drive = s.len() >= 2
            && s.as_bytes()[1] == b':'
            && s.as_bytes()[0].is_ascii_alphabetic()
            && matches!(s.as_bytes().get(2), None | Some(b'\\') | Some(b'/'));
        match s.split_once(':') {
            Some((host, root)) if !is_drive && !host.is_empty() && !host.contains(['/', '\\']) => {
                Target::Ssh {
                    host: host.into(),
                    root: if root.is_empty() { ".".into() } else { root.into() },
                }
            }
            _ => Target::Local { root: s.into() },
        }
    }

    pub fn label(&self) -> String {
        match self {
            Target::Local { root } => root.clone(),
            Target::Ssh { host, root } => format!("{host}:{root}"),
        }
    }

    fn local_path(root: &str, rel: &str) -> PathBuf {
        PathBuf::from(root).join(rel)
    }

    /// Common ssh options. Password/2FA prompts go through SSH_ASKPASS = this binary
    /// (see `ASKPASS_ENV`); unknown hosts are trusted on first use, changed keys still fail.
    pub fn ssh_base() -> Command {
        let mut c = Command::new("ssh");
        c.args([
            "-o", "StrictHostKeyChecking=accept-new",
            "-o", "ConnectTimeout=10",
            "-o", "ServerAliveInterval=15",
            "-o", "ServerAliveCountMax=4",
        ]);
        // Win32-OpenSSH has no connection multiplexing.
        #[cfg(unix)]
        // Socket paths max ~104 bytes; $TMPDIR can be longer. ~/.ssh is private and short.
        c.args(["-o", "ControlMaster=auto", "-o", "ControlPersist=10m", "-o", "ControlPath=~/.ssh/redit-%C"]);
        if let Ok(exe) = std::env::current_exe() {
            c.env("SSH_ASKPASS", exe).env("SSH_ASKPASS_REQUIRE", "force").env(ASKPASS_ENV, "1");
        }
        c
    }

    fn ssh_command(host: &str, root: &str, script: &str) -> Command {
        let script = format!("cd {} && {script}", sh_quote(root));
        let mut c = Self::ssh_base();
        // Remote login shell may be fish/nu: force sh.
        c.arg(host).arg(format!("sh -c {}", sh_quote(&script)));
        c
    }

    /// Serialize connection setup so N parallel ops on a cold host trigger one
    /// password prompt, not N. After the master is up, ops run in parallel.
    #[cfg(unix)]
    fn ensure_master(host: &str) {
        static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
        let _g = LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let up = Self::ssh_base().args(["-O", "check", host]).stderr(Stdio::null()).status();
        if !matches!(up, Ok(s) if s.success()) {
            let _ = Self::ssh_base().arg(host).arg("true").stdin(Stdio::null()).output();
        }
    }
    // ponytail: no multiplexing on Windows → password users get prompted per op; use keys/agent there.
    #[cfg(not(unix))]
    fn ensure_master(_host: &str) {}

    /// Spawn, feed stdin, return stdout (or stderr as the error).
    fn exec(mut c: Command, stdin: Option<&[u8]>) -> Result<(Vec<u8>, Vec<u8>), (Option<i32>, String)> {
        let mut child = c
            .stdin(if stdin.is_some() { Stdio::piped() } else { Stdio::null() })
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|e| (None, e.to_string()))?;
        if let Some(data) = stdin {
            child.stdin.take().unwrap().write_all(data).map_err(|e| (None, e.to_string()))?;
        }
        let out = child.wait_with_output().map_err(|e| (None, e.to_string()))?;
        if out.status.success() {
            Ok((out.stdout, out.stderr))
        } else {
            Err((out.status.code(), String::from_utf8_lossy(&out.stderr).trim().to_string()))
        }
    }

    /// Run a POSIX script on the remote root. Retries ssh transport failures (exit 255)
    /// with backoff so a dropped connection / sleeping laptop self-heals.
    fn ssh(host: &str, root: &str, script: &str, stdin: Option<&[u8]>) -> Result<(Vec<u8>, Vec<u8>), String> {
        let mut last = String::new();
        for attempt in 0..3 {
            if attempt > 0 {
                std::thread::sleep(Duration::from_millis(500 * attempt));
            }
            Self::ensure_master(host);
            match Self::exec(Self::ssh_command(host, root, script), stdin) {
                Ok(out) => return Ok(out),
                Err((code, err)) => {
                    last = err;
                    if code != Some(255) {
                        break;
                    }
                }
            }
        }
        Err(last)
    }

    /// `rel` is relative to root ("" = root). Dirs first, then alphabetical.
    pub fn list_dir(&self, rel: &str) -> Result<Vec<Entry>, String> {
        let mut v = match self {
            Target::Local { root } => std::fs::read_dir(Self::local_path(root, rel))
                .map_err(|e| e.to_string())?
                .filter_map(|e| e.ok())
                .map(|e| Entry {
                    name: e.file_name().to_string_lossy().into(),
                    // follows symlinks, like `ls -p`
                    is_dir: e.path().is_dir(),
                })
                .collect(),
            Target::Ssh { host, root } => {
                let dir = if rel.is_empty() { ".".into() } else { sh_quote(rel) };
                let (out, _) = Self::ssh(host, root, &format!("ls -1Ap {dir}"), None)?;
                String::from_utf8_lossy(&out)
                    .lines()
                    .map(|l| match l.strip_suffix('/') {
                        Some(d) => Entry { name: d.into(), is_dir: true },
                        None => Entry { name: l.into(), is_dir: false },
                    })
                    .collect::<Vec<_>>()
            }
        };
        sort_entries(&mut v);
        Ok(v)
    }

    pub fn read(&self, rel: &str) -> Result<String, String> {
        let bytes = match self {
            Target::Local { root } => std::fs::read(Self::local_path(root, rel)).map_err(|e| e.to_string())?,
            Target::Ssh { host, root } => Self::ssh(host, root, &format!("cat {}", sh_quote(rel)), None)?.0,
        };
        String::from_utf8(bytes).map_err(|_| "binary file".into())
    }

    /// Never leave a truncated file: write a temp file first, then swap it in.
    pub fn write(&self, rel: &str, data: &str) -> Result<(), String> {
        match self {
            Target::Local { root } => {
                let f = Self::local_path(root, rel);
                let mut tmp = f.clone().into_os_string();
                tmp.push(format!(".redit.{}", std::process::id()));
                let tmp = PathBuf::from(tmp);
                let res = (|| {
                    std::fs::write(&tmp, data)?;
                    if let Ok(m) = std::fs::metadata(&f) {
                        std::fs::set_permissions(&tmp, m.permissions())?;
                    }
                    std::fs::rename(&tmp, &f) // replaces existing on Unix and Windows
                })();
                if res.is_err() {
                    let _ = std::fs::remove_file(&tmp);
                }
                res.map_err(|e| e.to_string())
            }
            // Upload to temp, then `cat >` into place: keeps original owner/permissions.
            Target::Ssh { host, root } => {
                let f = sh_quote(rel);
                let script = format!(
                    "t=$(mktemp {f}.redit.XXXXXX) && cat > \"$t\" && cat \"$t\" > {f} && rm -f \"$t\""
                );
                Self::ssh(host, root, &script, Some(data.as_bytes())).map(|_| ())
            }
        }
    }

    /// Absolute workspace root on the machine it lives on (for LSP `file://` URIs).
    pub fn abs_root(&self) -> Result<String, String> {
        match self {
            Target::Local { root } => std::fs::canonicalize(root)
                .map(|p| p.to_string_lossy().into_owned())
                .map_err(|e| e.to_string()),
            Target::Ssh { host, root } => {
                let (out, _) = Self::ssh(host, root, "pwd -P", None)?;
                Ok(String::from_utf8_lossy(&out).trim().to_string())
            }
        }
    }

    /// Command running `argv` in the workspace root, locally or on the remote
    /// (remote: login shell so ~/.profile PATH applies, e.g. ~/.cargo/bin).
    /// Caller sets stdio and spawns. Blocking (may wait for the ssh master): call off the UI thread.
    pub fn process(&self, argv: &[&str]) -> Command {
        match self {
            Target::Local { root } => {
                let mut c = Command::new(argv[0]);
                c.args(&argv[1..]).current_dir(root);
                c
            }
            Target::Ssh { host, root } => {
                Self::ensure_master(host);
                let cmd = argv.iter().map(|a| sh_quote(a)).collect::<Vec<_>>().join(" ");
                let script = format!("cd {} && exec {cmd}", sh_quote(root));
                let mut c = Self::ssh_base();
                c.arg(host).arg(format!("sh -lc {}", sh_quote(&script)));
                c
            }
        }
    }

    /// Interactive shell in the workspace root, meant to run inside a PTY.
    pub fn shell(&self) -> Command {
        match self {
            Target::Local { root } => {
                #[cfg(unix)]
                let sh = std::env::var("SHELL").unwrap_or_else(|_| "/bin/sh".into());
                #[cfg(not(unix))]
                let sh = std::env::var("COMSPEC").unwrap_or_else(|_| "cmd.exe".into());
                let mut c = Command::new(sh);
                c.current_dir(root);
                c
            }
            Target::Ssh { host, root } => {
                Self::ensure_master(host);
                let mut c = Self::ssh_base();
                c.arg("-t").arg(host).arg(format!("cd {} && exec \"${{SHELL:-sh}}\" -l", sh_quote(root)));
                c
            }
        }
    }

    /// Fixed-string search across the workspace (skips .git/node_modules/target/.venv, binaries).
    pub fn search(&self, query: &str) -> Result<Vec<SearchHit>, String> {
        if query.is_empty() {
            return Ok(vec![]);
        }
        match self {
            Target::Local { root } => {
                let mut hits = vec![];
                search_dir(&PathBuf::from(root), "", query, &mut hits);
                Ok(hits)
            }
            Target::Ssh { host, root } => {
                let excl: String = SKIP_DIRS.iter().map(|d| format!(" --exclude-dir={d}")).collect();
                // --null: "path\0line:text" (GNU + BSD grep). head caps transfer.
                let script = format!(
                    "grep -rnIF --null{excl} -e {} . | head -n {MAX_HITS}",
                    sh_quote(query)
                );
                let (out, _) = Self::ssh(host, root, &script, None)?;
                Ok(String::from_utf8_lossy(&out)
                    .lines()
                    .filter_map(|l| {
                        let (path, rest) = l.split_once('\0')?;
                        let (line, text) = rest.split_once(':')?;
                        Some(SearchHit {
                            path: path.strip_prefix("./").unwrap_or(path).into(),
                            line: line.parse().ok()?,
                            text: text.into(),
                        })
                    })
                    .collect())
            }
        }
    }

    /// Modification times (unix secs) for change polling; None = missing.
    // ponytail: polling, not inotify. One cheap multiplexed round trip; a remote helper if it ever matters.
    pub fn mtimes(&self, rels: &[String]) -> Result<Vec<Option<i64>>, String> {
        match self {
            Target::Local { root } => Ok(rels
                .iter()
                .map(|r| {
                    let t = std::fs::metadata(Self::local_path(root, r)).ok()?.modified().ok()?;
                    Some(t.duration_since(std::time::UNIX_EPOCH).ok()?.as_secs() as i64)
                })
                .collect()),
            Target::Ssh { host, root } => {
                if rels.is_empty() {
                    return Ok(vec![]);
                }
                let files: Vec<String> = rels.iter().map(|r| sh_quote(r)).collect();
                // GNU stat -c, else BSD stat -f.
                let script = format!(
                    "for f in {}; do stat -c %Y \"$f\" 2>/dev/null || stat -f %m \"$f\" 2>/dev/null || echo -; done",
                    files.join(" ")
                );
                let (out, _) = Self::ssh(host, root, &script, None)?;
                Ok(String::from_utf8_lossy(&out).lines().map(|l| l.trim().parse().ok()).collect())
            }
        }
    }

    /// Forward localhost:`local` → remote localhost:`remote`.
    pub fn forward(&self, local: u16, remote: u16) -> Result<Forward, String> {
        let Target::Ssh { host, .. } = self else {
            return Err("port forwarding needs a remote target".into());
        };
        let spec = format!("{local}:localhost:{remote}");
        #[cfg(unix)]
        {
            Self::ensure_master(host);
            let mut c = Self::ssh_base();
            c.args(["-O", "forward", "-L", &spec, host]);
            Self::exec(c, None).map_err(|(_, e)| e)?;
            Ok(Forward { local, remote, host: host.clone(), spec })
        }
        #[cfg(not(unix))]
        {
            let child = Self::ssh_base()
                .args(["-N", "-o", "ExitOnForwardFailure=yes", "-L", &spec, host])
                .stdin(Stdio::null())
                .spawn()
                .map_err(|e| e.to_string())?;
            Ok(Forward { local, remote, host: host.clone(), spec, child })
        }
    }

    // --- git: shell out to `git`, locally or over ssh ---

    /// stdout + stderr on success (pull/push report progress on stderr).
    fn git(&self, args: &[&str]) -> Result<String, String> {
        let (out, err) = match self {
            Target::Local { root } => {
                let mut c = Command::new("git");
                c.args(args).current_dir(root);
                Self::exec(c, None).map_err(|(_, e)| e)?
            }
            Target::Ssh { host, root } => {
                let script = std::iter::once("git".to_string())
                    .chain(args.iter().map(|a| sh_quote(a)))
                    .collect::<Vec<_>>()
                    .join(" ");
                Self::ssh(host, root, &script, None)?
            }
        };
        Ok(format!("{}{}", String::from_utf8_lossy(&out), String::from_utf8_lossy(&err)))
    }

    pub fn git_branch(&self) -> Option<String> {
        self.git(&["rev-parse", "--abbrev-ref", "HEAD"]).ok().map(|s| s.trim().into())
    }

    /// (XY status code, path). `-z`: paths unquoted, NUL-separated.
    pub fn git_status(&self) -> Result<Vec<(String, String)>, String> {
        let out = self.git(&["status", "--porcelain=v1", "-z", "-uall"])?;
        let mut it = out.split('\0');
        let mut v = vec![];
        while let Some(rec) = it.next() {
            if rec.len() > 3 {
                let xy = &rec[..2];
                if xy.contains(['R', 'C']) {
                    it.next(); // rename/copy: next field is the old path
                }
                v.push((xy.to_string(), rec[3..].to_string()));
            }
        }
        Ok(v)
    }

    pub fn git_diff(&self, rel: &str) -> Result<String, String> {
        self.git(&["diff", "HEAD", "--", rel])
    }

    pub fn git_commit_all(&self, msg: &str) -> Result<String, String> {
        self.git(&["add", "-A"])?;
        self.git(&["commit", "-m", msg])
    }

    pub fn git_pull(&self) -> Result<String, String> {
        self.git(&["pull", "--ff-only"])
    }

    pub fn git_push(&self) -> Result<String, String> {
        self.git(&["push"])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmpdir(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("redit-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn parse_targets() {
        assert_eq!(
            Target::parse("me@box:/srv/app"),
            Target::Ssh { host: "me@box".into(), root: "/srv/app".into() }
        );
        assert_eq!(Target::parse("box:"), Target::Ssh { host: "box".into(), root: ".".into() });
        assert_eq!(Target::parse("/tmp/x"), Target::Local { root: "/tmp/x".into() });
        assert_eq!(Target::parse("./a:b"), Target::Local { root: "./a:b".into() });
        assert_eq!(Target::parse(r"C:\code"), Target::Local { root: r"C:\code".into() });
        assert_eq!(Target::parse("C:/code"), Target::Local { root: "C:/code".into() });
        assert_eq!(Target::parse("C:"), Target::Local { root: "C:".into() });
    }

    #[test]
    fn local_roundtrip_with_hostile_name() {
        let dir = tmpdir("fs");
        std::fs::create_dir_all(dir.join("sub")).unwrap();
        let t = Target::Local { root: dir.to_string_lossy().into() };
        let name = "it's $(rm -rf nope) a file.txt";
        t.write(name, "hello\n").unwrap();
        t.write(name, "bye\n").unwrap(); // overwrite existing
        assert_eq!(t.read(name).unwrap(), "bye\n");
        let ls = t.list_dir("").unwrap();
        assert_eq!(ls[0], Entry { name: "sub".into(), is_dir: true });
        assert!(ls.iter().any(|e| e.name == name && !e.is_dir));
        assert!(!ls.iter().any(|e| e.name.contains(".redit.")), "temp file leaked");
        t.write("sub/x.rs", "a\nneedle here\n").unwrap();
        std::fs::create_dir_all(dir.join("target")).unwrap();
        t.write("target/y.rs", "needle\n").unwrap(); // skipped dir
        assert_eq!(
            t.search("needle").unwrap(),
            vec![SearchHit { path: "sub/x.rs".into(), line: 2, text: "needle here".into() }]
        );
        let m = t.mtimes(&["sub/x.rs".into(), "missing".into()]).unwrap();
        assert!(m[0].is_some() && m[1].is_none());
        std::fs::remove_dir_all(dir).unwrap();
    }

    /// Real ssh round trip. `REDIT_TEST_HOST=ssh://me@127.0.0.1:2222 cargo test -- --ignored`
    #[test]
    #[ignore]
    fn remote_roundtrip() {
        let host = std::env::var("REDIT_TEST_HOST").expect("set REDIT_TEST_HOST");
        let dir = tmpdir("remote");
        let t = Target::Ssh { host, root: dir.to_string_lossy().into() };
        let name = "it's $(x) a file.txt";
        t.write(name, "hi\n").unwrap();
        assert_eq!(t.read(name).unwrap(), "hi\n");
        assert!(t.list_dir("").unwrap().iter().any(|e| e.name == name));
        assert_eq!(t.search("hi").unwrap()[0].path, name);
        assert!(t.mtimes(&[name.into()]).unwrap()[0].is_some());
        assert_eq!(t.abs_root().unwrap(), std::fs::canonicalize(&dir).unwrap().to_string_lossy());
        let out = t.process(&["echo", "a b"]).output().unwrap();
        assert_eq!(String::from_utf8_lossy(&out.stdout).trim(), "a b");
        t.git(&["init", "-q"]).unwrap();
        assert_eq!(t.git_status().unwrap()[0].1, name);
        let f = t.forward(18765, 22).unwrap();
        assert!(std::net::TcpStream::connect("127.0.0.1:18765").is_ok());
        f.stop();
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn local_git() {
        let dir = tmpdir("git");
        let t = Target::Local { root: dir.to_string_lossy().into() };
        t.git(&["init", "-q", "-b", "main"]).unwrap();
        t.git(&["config", "user.email", "t@t"]).unwrap();
        t.git(&["config", "user.name", "t"]).unwrap();
        t.write("a b.txt", "x\n").unwrap();
        assert_eq!(t.git_status().unwrap(), vec![("??".into(), "a b.txt".into())]);
        t.git_commit_all("first").unwrap();
        t.git(&["mv", "a b.txt", "c d.txt"]).unwrap();
        assert_eq!(t.git_status().unwrap(), vec![("R ".into(), "c d.txt".into())]);
        t.git_commit_all("it's a msg").unwrap();
        assert_eq!(t.git_branch().as_deref(), Some("main"));
        assert!(t.git_status().unwrap().is_empty());
        std::fs::remove_dir_all(dir).unwrap();
    }
}
