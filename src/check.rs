//! `linelapse --check`: compare indexed per-file line counts with the real blobs at HEAD and at
//! three intermediate commits. Binary files (NUL in the first 8000 bytes, git's own rule)
//! and submodules are skipped, as in the builder.
//!
//! Then compare the birth commit of every line of sampled files at HEAD with
//! `git blame --first-parent`: blame dates a line by when it reached the first-parent
//! history, the index's own definition, and follows whole-file renames. Counts alone
//! cannot catch a wrong birth date; this does.

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

/// Birth commit (as a first-parent position) of every line of `path` at HEAD, per git blame.
fn blame_histogram(
    repo: &Path,
    path: &str,
    pos: &HashMap<&str, u32>,
) -> Result<Vec<(u32, i64)>, String> {
    let out = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(["blame", "--first-parent", "-l", "-s", "HEAD", "--", path])
        .output()
        .map_err(|e| e.to_string())?;
    if !out.status.success() {
        return Err(format!("git blame failed on {path}"));
    }
    let mut h: std::collections::BTreeMap<u32, i64> = Default::default();
    for line in out.stdout.split(|&b| b == b'\n').filter(|l| !l.is_empty()) {
        let tok = line.split(|&b| b == b' ').next().unwrap_or(&[]);
        let tok = String::from_utf8_lossy(tok);
        // A boundary (root) commit is printed as '^' plus 39 hex digits.
        let sha = tok.trim_start_matches('^');
        let c = match pos.get(sha) {
            Some(&c) => c,
            None => {
                *pos.iter()
                    .find(|(h, _)| h.starts_with(sha) && sha.len() >= 39)
                    .ok_or_else(|| format!("blame names {sha}, not first-parent"))?
                    .1
            }
        };
        *h.entry(c).or_default() += 1;
    }
    Ok(h.into_iter().collect())
}

/// A sampled file and its indexed (birth commit, live lines) histogram.
type Pick<'a> = (u32, &'a Vec<(u32, i64)>);

/// Sampled files whose per-line birth commits differ from `git blame`, and the share of
/// lines that agree. `sample` = how many live files to check (evenly spread by path).
pub fn blame(repo: &Path, data: &Data, sample: usize) -> Result<(usize, usize, f64), String> {
    // Called after `run`, which leaves the engine at HEAD.
    let live: Vec<u32> = (0..data.meta.files.len())
        .filter(|&f| data.engine.file_counts()[f] > 0)
        .map(|f| f as u32)
        .collect();
    let step = live.len().div_ceil(sample.max(1)).max(1);
    let picked: Vec<u32> = live.iter().copied().step_by(step).collect();
    let ours = data.engine.birth_histograms(&picked);
    let pos: HashMap<&str, u32> = data
        .meta
        .commits
        .iter()
        .enumerate()
        .map(|(i, c)| (c.hash.as_str(), i as u32))
        .collect();
    let threads = std::thread::available_parallelism().map_or(4, |p| p.get());
    let chunks: Vec<Vec<Pick>> = picked
        .iter()
        .copied()
        .zip(ours.iter())
        .collect::<Vec<_>>()
        .chunks(picked.len().div_ceil(threads).max(1))
        .map(|c| c.to_vec())
        .collect();
    let results: Vec<Result<(usize, i64, i64), String>> = std::thread::scope(|s| {
        let hs: Vec<_> = chunks
            .into_iter()
            .map(|chunk| {
                let pos = &pos;
                s.spawn(move || {
                    let (mut bad, mut agree, mut total) = (0usize, 0i64, 0i64);
                    for (f, mine) in chunk {
                        let path = &data.meta.files[f as usize];
                        let theirs = blame_histogram(repo, path, pos)?;
                        let mut m: HashMap<u32, (i64, i64)> = HashMap::new();
                        for &(c, k) in mine {
                            m.entry(c).or_default().0 += k;
                        }
                        for &(c, k) in &theirs {
                            m.entry(c).or_default().1 += k;
                        }
                        let ok: i64 = m.values().map(|&(a, b)| a.min(b)).sum();
                        let n: i64 = theirs.iter().map(|e| e.1).sum();
                        if ok != n || mine.iter().map(|e| e.1).sum::<i64>() != n {
                            bad += 1;
                            if bad <= 3 {
                                println!("  blame differs on {path}: {ok}/{n} lines agree");
                            }
                        }
                        agree += ok;
                        total += n;
                    }
                    Ok((bad, agree, total))
                })
            })
            .collect();
        hs.into_iter()
            .map(|h| {
                h.join()
                    .unwrap_or_else(|_| Err("blame thread panicked".into()))
            })
            .collect()
    });
    let (mut bad, mut agree, mut total) = (0, 0, 0);
    for r in results {
        let (b, a, t) = r?;
        bad += b;
        agree += a;
        total += t;
    }
    let share = if total > 0 {
        agree as f64 / total as f64
    } else {
        1.0
    };
    println!(
        "blame at HEAD: {} files sampled, {} differ, {:.3}% of {} lines agree on birth commit",
        picked.len(),
        bad,
        100.0 * share,
        total
    );
    Ok((picked.len(), bad, share))
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
                // No detached background gc/maintenance outliving the test.
                "-c",
                "gc.auto=0",
                "-c",
                "maintenance.auto=false",
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
        // Rename with an edit, a swap of two files, and a rename whose old path is reused.
        let body = |tag: &str| -> Vec<u8> {
            (0..12)
                .map(|i| format!("{tag} line {i}\n"))
                .collect::<String>()
                .into_bytes()
        };
        write(&dir, "p.txt", &body("p"));
        write(&dir, "q.txt", &body("q"));
        write(&dir, "r.txt", &body("r"));
        git(&dir, &["add", "-A"]);
        git(&dir, &["commit", "-qm", "three files"]);
        std::fs::create_dir_all(dir.join("docs")).unwrap();
        git(&dir, &["mv", "p.txt", "docs/p.md"]);
        let mut p = body("p");
        p.extend_from_slice(b"one more\n");
        write(&dir, "docs/p.md", &p);
        git(&dir, &["mv", "q.txt", "tmp"]);
        git(&dir, &["mv", "r.txt", "q.txt"]);
        git(&dir, &["mv", "tmp", "r.txt"]);
        git(&dir, &["mv", "src/a.c", "src/b.c"]);
        write(&dir, "src/a.c", b"brand new\n");
        git(&dir, &["add", "-A"]);
        git(&dir, &["commit", "-qm", "renames"]);
        let tmp_cache = dir.join("cache");
        let progress = crate::build::Progress::default();
        crate::build::build(&dir, &tmp_cache, &progress).expect("build");
        let mut data = crate::app::load(&tmp_cache).expect("load");
        assert_eq!(data.engine.len(), 7, "first-parent commits");
        assert_eq!(
            crate::story::source_of(&data.meta.commits[3]),
            "branch side"
        );
        assert_eq!(super::run(&dir, &mut data).expect("check"), 0);
        // HEAD: src/b.c 5 + src/a.c 1 + with space 3 + é.md 3 + side/new.rs 2
        // + docs/p.md 13 + q.txt 12 + r.txt 12 = 51 live lines.
        assert_eq!(*data.engine.totals.last().unwrap(), 51);
        // p.txt -> docs/p.md keeps its 12 lines' birth (1 new line). git pairs only a
        // vanished path with a new one, so the q/r swap (24 lines) and src/a.c -> src/b.c
        // with a new src/a.c (5 lines) stay deaths plus births, as in `git blame`.
        // Born: 1 + 24 + 5 + 1 ("brand new") = 31. Dead: 24 + 5 = 29.
        assert_eq!(data.engine.churn.last(), Some(&(31, 29)));
        let (files, bad, share) = super::blame(&dir, &data, 100).expect("blame");
        assert_eq!((files, bad, share), (8, 0, 1.0));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
