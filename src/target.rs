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

/// Single-quote for POSIX sh. Paths come from the user / remote listings: never trust them.
pub fn sh_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', r"'\''"))
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

    fn ssh_command(host: &str, root: &str, script: &str) -> Command {
        let script = format!("cd {} && {script}", sh_quote(root));
        let mut c = Command::new("ssh");
        c.args([
            "-o", "BatchMode=yes", // GUI can't answer prompts; needs key/agent
            "-o", "ConnectTimeout=10",
            "-o", "ServerAliveInterval=15",
            "-o", "ServerAliveCountMax=4",
        ]);
        // Win32-OpenSSH has no connection multiplexing.
        #[cfg(unix)]
        c.args(["-o", "ControlMaster=auto", "-o", "ControlPersist=10m"])
            .arg("-o")
            .arg(format!("ControlPath={}", std::env::temp_dir().join("redit-ssh-%C").display()));
        // Remote login shell may be fish/nu: force sh.
        c.arg(host).arg(format!("sh -c {}", sh_quote(&script)));
        c
    }

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
