//! `linelapse --check`: compare indexed per-file line counts with the real blobs at HEAD and at
//! three intermediate commits. Binary files (NUL in the first 8000 bytes, git's own rule)
//! and submodules are skipped, as in the builder.

use std::collections::HashMap;
use std::io::Write;
use std::path::Path;
use std::process::{Command, Stdio};

use crate::app::Data;

fn real_counts(repo: &Path, rev: &str) -> Result<HashMap<String, i64>, String> {
    let out = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(["-c", "core.quotePath=false", "ls-tree", "-r", "-z", rev])
        .output()
        .map_err(|e| e.to_string())?;
    let mut blobs = Vec::new();
    for rec in out.stdout.split(|&b| b == 0).filter(|r| !r.is_empty()) {
        let s = String::from_utf8_lossy(rec);
        let (head, path) = s.split_once('\t').ok_or("unreadable ls-tree output")?;
        let f: Vec<&str> = head.split(' ').collect();
        if f.get(1) == Some(&"blob") {
            blobs.push((path.to_string(), f[2].to_string()));
        }
    }
    let mut child = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(["cat-file", "--batch"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .map_err(|e| e.to_string())?;
    let mut stdin = child.stdin.take().expect("stdin");
    let ids: Vec<String> = blobs.iter().map(|b| b.1.clone()).collect();
    let feeder = std::thread::spawn(move || {
        for id in ids {
            let _ = writeln!(stdin, "{id}");
        }
    });
    let raw = child.wait_with_output().map_err(|e| e.to_string())?.stdout;
    let _ = feeder.join();
    let mut counts = HashMap::new();
    let mut i = 0;
    for (path, _) in &blobs {
        let nl = i + raw[i..]
            .iter()
            .position(|&b| b == b'\n')
            .ok_or("truncated cat-file output")?;
        let hdr = String::from_utf8_lossy(&raw[i..nl]);
        let size: usize = hdr
            .split(' ')
            .nth(2)
            .and_then(|s| s.parse().ok())
            .ok_or("bad blob size")?;
        let body = &raw[nl + 1..nl + 1 + size];
        i = nl + 1 + size + 1;
        if body[..body.len().min(8000)].contains(&0) {
            continue;
        }
        let n = body.iter().filter(|&&b| b == b'\n').count() as i64
            + i64::from(!body.is_empty() && !body.ends_with(b"\n"));
        if n > 0 {
            counts.insert(path.clone(), n);
        }
    }
    Ok(counts)
}

/// Returns the number of mismatching files over all checked commits.
pub fn run(repo: &Path, data: &mut Data) -> Result<usize, String> {
    let n = data.engine.len();
    let mut bad_total = 0;
    for t in [n / 3, n / 2, 2 * n / 3, n - 1] {
        data.engine.seek(t);
        let real = real_counts(repo, &data.meta.commits[t].hash)?;
        let mut idx = HashMap::new();
        for (f, path) in data.meta.files.iter().enumerate() {
            let c = data.engine.agg(f, f + 1).count;
            if c != 0 {
                idx.insert(path.clone(), c);
            }
        }
        let mut bad: Vec<&String> = real
            .keys()
            .chain(idx.keys())
            .filter(|p| real.get(*p) != idx.get(*p))
            .collect();
        bad.sort();
        bad.dedup();
        println!(
            "commit {}/{} {}: real files={} indexed={} mismatches={}",
            t + 1,
            n,
            &data.meta.commits[t].hash[..12],
            real.len(),
            idx.len(),
            bad.len()
        );
        for p in bad.iter().take(5) {
            println!(
                "  mismatch {p}: real={:?} indexed={:?}",
                real.get(*p),
                idx.get(*p)
            );
        }
        bad_total += bad.len();
    }
    Ok(bad_total)
}

#[cfg(test)]
mod tests {
    use std::path::Path;
    use std::process::Command;

    fn git(dir: &Path, args: &[&str]) {
        let ok = Command::new("git")
            .arg("-C")
            .arg(dir)
            .args([
                "-c",
                "user.name=T",
                "-c",
                "user.email=t@t",
                "-c",
                "commit.gpgsign=false",
            ])
            .args(args)
            .env("GIT_AUTHOR_DATE", "2020-01-01T00:00:00Z")
            .env("GIT_COMMITTER_DATE", "2020-01-01T00:00:00Z")
            .status()
            .expect("git")
            .success();
        assert!(ok, "git {args:?}");
    }

    fn write(dir: &Path, f: &str, s: &[u8]) {
        let p = dir.join(f);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, s).unwrap();
    }

    /// Whole pipeline on a repo full of traps; the index must match the blobs exactly.
    #[test]
    fn index_matches_blobs_on_tricky_repo() {
        let dir = std::env::temp_dir().join(format!("linelapse-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        git(&dir, &["init", "-q", "-b", "main"]);
        write(&dir, "a.c", b"1\n2\n3\n-- x\n5\n");
        write(&dir, "dir/with space.txt", b"a\nb\n");
        write(&dir, "café/é.md", b"x\ny");
        write(&dir, "bin.dat", b"\x00\x01\x02\n\x03");
        git(&dir, &["add", "-A"]);
        git(&dir, &["commit", "-qm", "init"]);
        write(&dir, "a.c", b"1\n3\n5\n6\n");
        write(&dir, "café/é.md", b"x\ny\nz\n");
        git(&dir, &["commit", "-qam", "edit"]);
        git(&dir, &["checkout", "-qb", "side"]);
        write(&dir, "side/new.rs", b"fn a(){}\nfn b(){}\n");
        write(&dir, "dir/with space.txt", b"a\nB\nc\n");
        git(&dir, &["add", "-A"]);
        git(&dir, &["commit", "-qm", "side work"]);
        git(&dir, &["checkout", "-q", "main"]);
        write(&dir, "a.c", b"0\n1\n3\n5\n6\n");
        git(&dir, &["commit", "-qam", "main work"]);
        git(
            &dir,
            &[
                "merge",
                "-q",
                "--no-ff",
                "-m",
                "Merge branch 'side'",
                "side",
            ],
        );
        std::fs::create_dir_all(dir.join("src")).unwrap();
        git(&dir, &["mv", "a.c", "src/a.c"]);
        git(&dir, &["rm", "-q", "bin.dat"]);
        git(&dir, &["commit", "-qm", "move"]);
        let tmp_cache = dir.join("cache");
        let progress = crate::build::Progress::default();
        crate::build::build(&dir, &tmp_cache, &progress).expect("build");
        let mut data = crate::app::load(&tmp_cache).expect("load");
        assert_eq!(data.engine.len(), 5, "first-parent commits");
        assert_eq!(
            crate::story::source_of(&data.meta.commits[3]),
            "branch side"
        );
        assert_eq!(super::run(&dir, &mut data).expect("check"), 0);
        // HEAD: src/a.c 5 + with space 3 + é.md 3 + side/new.rs 2 = 13 live lines.
        assert_eq!(*data.engine.totals.last().unwrap(), 13);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
