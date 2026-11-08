//! Accept a repository URL as well as a local path: clone it (bare) into the cache on
//! first use, fetch on later runs.
//!
//! Security: the URL is untrusted input handed to git. Only plain transports are
//! accepted (https, http, git, ssh, file, scp-like `git@host:path`), git's `ext::`
//! transport (which runs commands) is disabled explicitly, the URL goes after `--` so it
//! can never be read as an option, and terminal prompts are off so a private repo fails
//! instead of hanging the window.

use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Mutex;

const SCHEMES: [&str; 5] = ["https://", "http://", "git://", "ssh://", "file://"];

/// True if `arg` looks like a remote (URL or scp-like), not a local path.
pub fn is_url(arg: &str) -> bool {
    SCHEMES.iter().any(|s| arg.starts_with(s)) || is_scp_like(arg)
}

fn is_scp_like(arg: &str) -> bool {
    // user@host:path, without a scheme and without a slash before the colon.
    match (arg.find('@'), arg.find(':')) {
        (Some(at), Some(colon)) => at < colon && !arg[..colon].contains('/'),
        _ => false,
    }
}

/// Where the bare clone of `url` lives: `<cache>/repos/<host>/<path>.git`.
pub fn clone_dir(base: &Path, url: &str) -> PathBuf {
    let mut s = url;
    for scheme in SCHEMES {
        s = s.strip_prefix(scheme).unwrap_or(s);
    }
    if let Some(at) = s.find('@').filter(|&at| s[..at].chars().all(|c| c != '/')) {
        s = &s[at + 1..]; // drop user@
    }
    let s = s.replace(':', "/");
    let clean: Vec<String> = s
        .trim_end_matches('/')
        .trim_end_matches(".git")
        .split('/')
        .filter(|p| !p.is_empty() && *p != "." && *p != "..")
        .map(|p| {
            p.chars()
                .map(|c| {
                    if c.is_ascii_alphanumeric() || "._-".contains(c) {
                        c
                    } else {
                        '_'
                    }
                })
                .collect()
        })
        .collect();
    let mut dir = base.join("repos");
    for p in &clean {
        dir.push(p);
    }
    dir.set_extension("git");
    dir
}

fn git_quiet() -> Command {
    let mut c = Command::new("git");
    c.env("GIT_TERMINAL_PROMPT", "0")
        .args(["-c", "protocol.ext.allow=never"]);
    c
}

/// Run git, streaming its stderr progress ("Receiving objects: 42%") into `phase`.
fn run_with_progress(mut cmd: Command, label: &str, phase: &Mutex<String>) -> Result<(), String> {
    let mut child = cmd
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("git not found: {e}"))?;
    let mut err = child.stderr.take().expect("stderr");
    let mut buf = [0u8; 4096];
    let mut line = Vec::new();
    let mut last = String::new();
    loop {
        let k = err.read(&mut buf).map_err(|e| e.to_string())?;
        if k == 0 {
            break;
        }
        for &b in &buf[..k] {
            if b == b'\r' || b == b'\n' {
                if !line.is_empty() {
                    last = String::from_utf8_lossy(&line).trim().to_string();
                    *phase.lock().expect("phase") = format!("{label}: {last}");
                    line.clear();
                }
            } else {
                line.push(b);
            }
        }
    }
    let status = child.wait().map_err(|e| e.to_string())?;
    if !status.success() {
        return Err(format!("{label} failed: {last}"));
    }
    Ok(())
}

/// Local path for `arg`: itself if it is a path, else a fresh or updated bare clone.
pub fn resolve(arg: &str, base: &Path, phase: &Mutex<String>) -> Result<PathBuf, String> {
    if !is_url(arg) {
        return Ok(PathBuf::from(arg));
    }
    if arg.starts_with('-') {
        return Err("invalid repository URL".into());
    }
    let dir = clone_dir(base, arg);
    if dir.join("HEAD").exists() {
        let mut cmd = git_quiet();
        cmd.arg("-C").arg(&dir).args([
            "fetch",
            "--progress",
            "--prune",
            "origin",
            "+refs/heads/*:refs/heads/*",
        ]);
        run_with_progress(cmd, "Fetching", phase)?;
        return Ok(dir);
    }
    let parent = dir.parent().ok_or("bad clone path")?;
    std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    let tmp = dir.with_extension("partial");
    let _ = std::fs::remove_dir_all(&tmp);
    let mut cmd = git_quiet();
    cmd.args(["clone", "--bare", "--progress", "--", arg])
        .arg(&tmp);
    run_with_progress(cmd, "Cloning", phase)?;
    std::fs::rename(&tmp, &dir).map_err(|e| e.to_string())?;
    Ok(dir)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn url_detection() {
        assert!(is_url("https://github.com/torvalds/linux"));
        assert!(is_url("git@github.com:torvalds/linux.git"));
        assert!(is_url("file:///tmp/x"));
        assert!(!is_url("."));
        assert!(!is_url("../linux"));
        assert!(!is_url("/home/me/src/linux"));
        assert!(!is_url("./dir/a@b:c")); // slash before the colon: a path
        assert!(!is_url("ext::sh -c touch% /tmp/pwned"));
    }

    #[test]
    fn clone_dirs_stay_inside_the_cache() {
        let base = Path::new("/c");
        assert_eq!(
            clone_dir(base, "https://github.com/torvalds/linux.git"),
            Path::new("/c/repos/github.com/torvalds/linux.git")
        );
        assert_eq!(
            clone_dir(base, "git@github.com:a/b"),
            Path::new("/c/repos/github.com/a/b.git")
        );
        let evil = clone_dir(base, "https://h/../../../etc/x y");
        assert!(evil.starts_with("/c/repos/"), "{evil:?}");
        assert!(!evil.to_string_lossy().contains(".."));
    }

    #[test]
    fn clones_then_fetches_a_local_remote() {
        let root = std::env::temp_dir().join(format!("linelapse-remote-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let src = root.join("src");
        std::fs::create_dir_all(&src).unwrap();
        let git = |args: &[&str]| {
            assert!(
                Command::new("git")
                    .arg("-C")
                    .arg(&src)
                    .args(["-c", "user.name=T", "-c", "user.email=t@t"])
                    .args(["-c", "commit.gpgsign=false", "-c", "gc.auto=0"])
                    .args(args)
                    .status()
                    .unwrap()
                    .success()
            )
        };
        git(&["init", "-q", "-b", "main"]);
        std::fs::write(src.join("a"), "1\n").unwrap();
        git(&["add", "-A"]);
        git(&["commit", "-qm", "one"]);
        let url = format!("file://{}", src.display());
        let phase = Mutex::new(String::new());
        let dir = resolve(&url, &root.join("cache"), &phase).expect("clone");
        assert!(dir.join("HEAD").exists());
        std::fs::write(src.join("a"), "1\n2\n").unwrap();
        git(&["commit", "-qam", "two"]);
        let again = resolve(&url, &root.join("cache"), &phase).expect("fetch");
        assert_eq!(dir, again);
        let n = Command::new("git")
            .arg("-C")
            .arg(&dir)
            .args(["rev-list", "--count", "HEAD"])
            .output()
            .unwrap();
        assert_eq!(String::from_utf8_lossy(&n.stdout).trim(), "2");
        let _ = std::fs::remove_dir_all(&root);
    }
}
