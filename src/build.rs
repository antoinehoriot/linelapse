//! Build the line-interval event streams from a git repository.
//!
//! Walks first-parent history oldest first. Every line gets a birth commit (the mainline
//! commit that introduced it) and possibly a death commit. Instead of one record per line,
//! events are run-length encoded:
//!   births: (file, n) per (commit, file)          -- all n lines are born at that commit
//!   deaths: (file, birth, n) per (commit, file, birth run)
//!
//! Decisions (why, so they are not relitigated):
//! - We shell out to `git log -p -U0` instead of using gitoxide's diff: git's output was
//!   verified exact on the whole Linux history (0 mismatches over 97 316 files), and
//!   splitting the history in ranges lets us run one git process per core.
//! - Renames are followed (`-M50% -l0`, set explicitly so the user's git config cannot
//!   change the index): lines of a renamed file keep their birth commit and are recorded
//!   as moves, not as a death plus a birth. On Linux 28% of all "deaths" were renames
//!   (13.4 M of 47.7 M lines), which skewed ages, half-lives and per-author numbers.
//!   `-l0` lifts git's 1000-file limit on inexact detection; the full Linux scan with it
//!   costs about 2 s, and gave the same 35 284 renames as the default limit.
//!   Copies (`-C`) are not followed: a copy duplicates lines, and which copy keeps the
//!   original birth would be arbitrary.
//! - Binary files and submodules produce no line events.
//! - File ids are assigned in sorted path order, so every directory is a contiguous id
//!   range; subtree queries become range checks.

use std::collections::HashMap;
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

pub const FORMAT: u32 = 2;

#[derive(Clone, serde::Serialize, serde::Deserialize)]
pub struct CommitMeta {
    pub hash: String,
    pub time: i64,
    pub merge: bool,
    pub author: String,
    pub subject: String,
}

#[derive(serde::Serialize, serde::Deserialize)]
pub struct Meta {
    pub format: u32,
    pub repo: String,
    pub head: String,
    pub commits: Vec<CommitMeta>,
    pub files: Vec<String>,
}

/// Progress shared with the UI while building.
#[derive(Default)]
pub struct Progress {
    pub total: AtomicUsize,
    pub done: AtomicUsize,
}

struct FilePatch {
    path: Box<str>,
    /// Old path when git paired this file as a rename; hunks then apply to its lines.
    from: Option<Box<str>>,
    /// (old_start, old_count, new_count) in git's 1-based hunk coordinates.
    hunks: Vec<(u32, u32, u32)>,
}

fn git(repo: &Path) -> Command {
    let mut c = Command::new("git");
    c.arg("-C").arg(repo).args([
        "-c",
        "core.quotePath=false",
        "-c",
        "log.showSignature=false",
        "-c",
        "color.ui=never",
    ]);
    c
}

fn git_out(repo: &Path, args: &[&str]) -> Result<String, String> {
    let o = git(repo)
        .args(args)
        .output()
        .map_err(|e| format!("git not found: {e}"))?;
    if !o.status.success() {
        return Err(String::from_utf8_lossy(&o.stderr).trim().to_string());
    }
    Ok(String::from_utf8_lossy(&o.stdout).into_owned())
}

pub fn head(repo: &Path) -> Result<String, String> {
    Ok(git_out(repo, &["rev-parse", "HEAD"])?.trim().to_string())
}

/// `<user cache>/linelapse`: indexes and clones of remote repositories live here.
pub fn cache_base() -> PathBuf {
    std::env::var_os("XDG_CACHE_HOME")
        .map(PathBuf::from)
        .or_else(|| {
            let home = PathBuf::from(std::env::var_os("HOME")?);
            Some(if cfg!(target_os = "macos") {
                home.join("Library/Caches")
            } else {
                home.join(".cache")
            })
        })
        .unwrap_or_else(std::env::temp_dir)
        .join("linelapse")
}

pub fn cache_dir(repo: &Path) -> PathBuf {
    let canon = repo.canonicalize().unwrap_or_else(|_| repo.to_path_buf());
    let name = canon
        .file_name()
        .map(|n| n.to_string_lossy().trim_end_matches(".git").to_string())
        .unwrap_or_else(|| "repo".into());
    // FNV-1a of the canonical path keeps two same-named repos apart.
    let mut h: u64 = 0xcbf29ce484222325;
    for b in canon.to_string_lossy().bytes() {
        h = (h ^ b as u64).wrapping_mul(0x100000001b3);
    }
    cache_base().join(format!("{name}-{h:016x}"))
}

/// Undo git's C-style quoting of unusual paths ("a/caf\303\251" -> a/café).
fn unquote(raw: &[u8]) -> Vec<u8> {
    if raw.len() < 2 || raw[0] != b'"' || raw[raw.len() - 1] != b'"' {
        return raw.to_vec();
    }
    let s = &raw[1..raw.len() - 1];
    let mut out = Vec::with_capacity(s.len());
    let mut i = 0;
    while i < s.len() {
        if s[i] == b'\\' && i + 1 < s.len() {
            let c = s[i + 1];
            if c.is_ascii_digit() && i + 4 <= s.len() {
                let oct = std::str::from_utf8(&s[i + 1..i + 4]).ok();
                if let Some(v) = oct.and_then(|o| u8::from_str_radix(o, 8).ok()) {
                    out.push(v);
                    i += 4;
                    continue;
                }
            }
            out.push(match c {
                b'n' => b'\n',
                b't' => b'\t',
                b'a' => 7,
                b'b' => 8,
                b'f' => 12,
                b'v' => 11,
                b'r' => b'\r',
                other => other,
            });
            i += 2;
        } else {
            out.push(s[i]);
            i += 1;
        }
    }
    out
}

/// Path from a `--- a/x` / `+++ b/x` line; `None` for /dev/null.
fn header_path(line: &[u8]) -> Option<Box<str>> {
    let mut p = &line[4..];
    while let [rest @ .., b'\n' | b'\r' | b'\t'] = p {
        p = rest; // git appends \t after names containing spaces
    }
    if p == b"/dev/null" {
        return None;
    }
    let p = unquote(p);
    let p = p.get(2..).unwrap_or(&[]); // strip a/ or b/
    Some(String::from_utf8_lossy(p).into())
}

/// Path from a `rename from x` / `rename to x` line (no a/ b/ prefix, maybe quoted).
fn rename_path(line: &[u8], prefix: &[u8]) -> Option<Box<str>> {
    let mut p = line.strip_prefix(prefix)?;
    while let [rest @ .., b'\n' | b'\r'] = p {
        p = rest;
    }
    Some(String::from_utf8_lossy(&unquote(p)).into())
}

fn parse_num(s: &[u8]) -> Option<u32> {
    std::str::from_utf8(s).ok()?.parse().ok()
}

/// Parse `@@ -a[,b] +c[,d] @@` into (a, b, d).
fn parse_hunk(line: &[u8]) -> Option<(u32, u32, u32)> {
    let s = line.strip_prefix(b"@@ -")?;
    let end = s.windows(3).position(|w| w == b" @@")?;
    let s = &s[..end];
    let sp = s.iter().position(|&c| c == b' ')?;
    let (old, new) = (&s[..sp], s[sp + 1..].strip_prefix(b"+")?);
    let split = |x: &[u8]| -> Option<(u32, u32)> {
        match x.iter().position(|&c| c == b',') {
            Some(i) => Some((parse_num(&x[..i])?, parse_num(&x[i + 1..])?)),
            None => Some((parse_num(x)?, 1)),
        }
    };
    let (a, b) = split(old)?;
    let (_, d) = split(new)?;
    Some((a, b, d))
}

/// Run `git log -p` over one first-parent range and parse it. `expect` is the exact
/// list of commits the range must produce, which catches any range mistake.
fn parse_range(
    repo: &Path,
    from: Option<&str>,
    to: &str,
    expect: &[String],
) -> Result<Vec<Vec<FilePatch>>, String> {
    let mut cmd = git(repo);
    cmd.args([
        "log",
        "--first-parent",
        "--diff-merges=first-parent",
        "--reverse",
        "-p",
        "-U0",
        "-M50%",
        "-l0",
        "--no-color",
        "--no-ext-diff",
        "--no-textconv",
        "--no-notes",
        "--src-prefix=a/",
        "--dst-prefix=b/",
        "--format=%x00%H",
        to,
    ]);
    if let Some(f) = from {
        cmd.arg(format!("^{f}"));
    }
    let mut child = cmd
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|e| e.to_string())?;
    let mut rd = BufReader::with_capacity(1 << 20, child.stdout.take().expect("stdout"));

    let mut commits: Vec<Vec<FilePatch>> = Vec::with_capacity(expect.len());
    let mut line = Vec::with_capacity(256);
    let mut rem: u32 = 0; // content lines left in the current hunk
    let mut minus: Option<Box<str>> = None;
    let mut rename_from: Option<Box<str>> = None;
    let mut skip = false; // submodule entry
    let mut open = false; // current file already pushed
    loop {
        line.clear();
        if rd.read_until(b'\n', &mut line).map_err(|e| e.to_string())? == 0 {
            break;
        }
        if rem > 0 {
            match line.first() {
                Some(b'+' | b'-') => {
                    rem -= 1;
                    continue;
                }
                Some(b'\\') => continue,
                _ => rem = 0,
            }
        }
        if line.first() == Some(&0) {
            let h = String::from_utf8_lossy(&line[1..]).trim().to_string();
            if expect.get(commits.len()) != Some(&h) {
                let _ = child.kill();
                return Err(format!(
                    "unexpected commit from git: {h} at position {}",
                    commits.len()
                ));
            }
            commits.push(Vec::new());
            continue;
        }
        if line.starts_with(b"diff --git ") {
            open = false;
            skip = false;
            minus = None;
            rename_from = None;
            continue;
        }
        // A rename opens the file entry by itself: a pure rename has no ---/+++ lines.
        if !open && let Some(p) = rename_path(&line, b"rename from ") {
            rename_from = Some(p);
            continue;
        }
        if !open
            && let Some(p) = rename_path(&line, b"rename to ")
            && let (Some(from), Some(c)) = (rename_from.take(), commits.last_mut())
        {
            c.push(FilePatch {
                path: p,
                from: Some(from),
                hunks: Vec::new(),
            });
            open = true;
            continue;
        }
        if line.starts_with(b"index ") && line.trim_ascii_end().ends_with(b" 160000")
            || line.starts_with(b"new file mode 160000")
            || line.starts_with(b"deleted file mode 160000")
        {
            skip = true;
            continue;
        }
        if line.starts_with(b"--- ") && !open {
            minus = header_path(&line);
            continue;
        }
        if line.starts_with(b"+++ ") && !open {
            let path = header_path(&line).or_else(|| minus.take());
            if let (Some(path), Some(c), false) = (path, commits.last_mut(), skip) {
                c.push(FilePatch {
                    path,
                    from: None,
                    hunks: Vec::new(),
                });
                open = true;
            }
            continue;
        }
        if line.starts_with(b"@@ ")
            && let Some((a, b, d)) = parse_hunk(&line)
        {
            rem = b + d;
            if open
                && !skip
                && let Some(f) = commits.last_mut().and_then(|c| c.last_mut())
            {
                f.hunks.push((a, b, d));
            }
        }
    }
    let status = child.wait().map_err(|e| e.to_string())?;
    if !status.success() {
        return Err(format!("git log failed ({status})"));
    }
    if commits.len() != expect.len() {
        return Err(format!(
            "git returned {} commits, expected {}",
            commits.len(),
            expect.len()
        ));
    }
    Ok(commits)
}

struct Out {
    /// Hunks that did not fit the tracked file state. Must stay 0; non-zero means a parse bug.
    anomalies: u64,
    births: Vec<[u32; 2]>,
    birth_off: Vec<u32>,
    deaths: Vec<[u32; 3]>,
    death_off: Vec<u32>,
    /// (from file, to file, birth commit, n): lines carried over by a rename.
    moves: Vec<[u32; 4]>,
    move_off: Vec<u32>,
}

pub fn build(repo: &Path, out_dir: &Path, progress: &Progress) -> Result<(), String> {
    let head = head(repo)?;
    let log = git_out(
        repo,
        &[
            "log",
            "--first-parent",
            "--reverse",
            "--no-show-signature",
            "--format=%H%x1f%ct%x1f%P%x1f%an%x1f%s%x1e",
            "HEAD",
        ],
    )?;
    let commits: Vec<CommitMeta> = log
        .split('\x1e')
        .filter_map(|r| {
            let r = r.trim_start_matches('\n');
            let f: Vec<&str> = r.splitn(5, '\x1f').collect();
            (f.len() == 5).then(|| CommitMeta {
                hash: f[0].to_string(),
                time: f[1].parse().unwrap_or(0),
                merge: f[2].split_whitespace().count() > 1,
                author: f[3].to_string(),
                subject: f[4].chars().take(160).collect(),
            })
        })
        .collect();
    if commits.is_empty() {
        return Err("no commits".into());
    }
    let n = commits.len();
    progress.total.store(n, Ordering::Relaxed);
    let hashes: Arc<Vec<String>> = Arc::new(commits.iter().map(|c| c.hash.clone()).collect());

    let threads = std::thread::available_parallelism().map_or(4, |p| p.get());
    let nchunks = (threads * 6).min(n).max(1);
    let bounds: Vec<(usize, usize)> = (0..nchunks)
        .map(|k| (k * n / nchunks, (k + 1) * n / nchunks))
        .collect();

    let mut ids: HashMap<Box<str>, u32> = HashMap::new();
    let mut paths: Vec<Box<str>> = Vec::new();
    let mut state: Vec<Vec<u32>> = Vec::new(); // birth commit of every live line, per file
    let mut o = Out {
        anomalies: 0,
        births: Vec::new(),
        birth_off: vec![0],
        deaths: Vec::new(),
        death_off: vec![0],
        moves: Vec::new(),
        move_off: vec![0],
    };
    let mut ci = 0usize;

    // Waves of `threads` chunks parsed in parallel, then applied in order (bounded memory).
    for wave in bounds.chunks(threads) {
        let handles: Vec<_> = wave
            .iter()
            .map(|&(s, e)| {
                let repo = repo.to_path_buf();
                let hashes = Arc::clone(&hashes);
                std::thread::spawn(move || {
                    let from = (s > 0).then(|| hashes[s - 1].as_str());
                    parse_range(&repo, from, &hashes[e - 1], &hashes[s..e])
                })
            })
            .collect();
        for h in handles {
            let chunk = h
                .join()
                .map_err(|_| "parser thread panicked".to_string())??;
            for files in chunk {
                apply_commit(ci as u32, files, &mut ids, &mut paths, &mut state, &mut o);
                ci += 1;
                o.birth_off.push(o.births.len() as u32);
                o.death_off.push(o.deaths.len() as u32);
                o.move_off.push(o.moves.len() as u32);
                progress.done.store(ci, Ordering::Relaxed);
            }
        }
    }
    debug_assert_eq!(ci, n);
    if o.anomalies > 0 {
        return Err(format!(
            "{} inconsistent hunks: index not reliable",
            o.anomalies
        ));
    }

    // Renumber files in sorted path order so each directory is a contiguous id range.
    let mut order: Vec<u32> = (0..paths.len() as u32).collect();
    order.sort_by(|&a, &b| paths[a as usize].cmp(&paths[b as usize]));
    let mut remap = vec![0u32; paths.len()];
    for (new, &old) in order.iter().enumerate() {
        remap[old as usize] = new as u32;
    }
    for b in &mut o.births {
        b[0] = remap[b[0] as usize];
    }
    for d in &mut o.deaths {
        d[0] = remap[d[0] as usize];
    }
    for m in &mut o.moves {
        m[0] = remap[m[0] as usize];
        m[1] = remap[m[1] as usize];
    }
    let files: Vec<String> = order
        .iter()
        .map(|&i| paths[i as usize].to_string())
        .collect();

    let tmp = out_dir.with_extension("tmp");
    let _ = std::fs::remove_dir_all(&tmp);
    std::fs::create_dir_all(&tmp).map_err(|e| e.to_string())?;
    write_u32s(&tmp.join("births.u32"), o.births.as_flattened())?;
    write_u32s(&tmp.join("birth_off.u32"), &o.birth_off)?;
    write_u32s(&tmp.join("deaths.u32"), o.deaths.as_flattened())?;
    write_u32s(&tmp.join("death_off.u32"), &o.death_off)?;
    write_u32s(&tmp.join("moves.u32"), o.moves.as_flattened())?;
    write_u32s(&tmp.join("move_off.u32"), &o.move_off)?;
    let meta = Meta {
        format: FORMAT,
        repo: repo
            .canonicalize()
            .unwrap_or_else(|_| repo.to_path_buf())
            .to_string_lossy()
            .into(),
        head,
        commits,
        files,
    };
    let json = serde_json::to_vec(&meta).map_err(|e| e.to_string())?;
    std::fs::write(tmp.join("meta.json"), json).map_err(|e| e.to_string())?;
    let _ = std::fs::remove_dir_all(out_dir);
    std::fs::rename(&tmp, out_dir).map_err(|e| e.to_string())?;
    Ok(())
}

/// (old_start, old_count, new_count) per hunk, as in `FilePatch`.
type Hunks = Vec<(u32, u32, u32)>;

fn apply_commit(
    c: u32,
    files: Vec<FilePatch>,
    ids: &mut HashMap<Box<str>, u32>,
    paths: &mut Vec<Box<str>>,
    state: &mut Vec<Vec<u32>>,
    o: &mut Out,
) {
    // Take every rename source first: in one commit a path can be both a source and a
    // destination (A -> B with B -> A, or A -> B with a new A).
    let mut sources: Vec<(u32, Vec<u32>)> = Vec::new();
    // (destination, rename source, hunks), applied after every source was taken.
    let mut plan: Vec<(u32, Option<u32>, Hunks)> = Vec::new();
    for fp in files {
        // A source that never had lines (binary, empty, submodule) carries nothing: treat
        // the destination as a plain file, so line-less paths still get no id.
        let from = fp
            .from
            .and_then(|f| ids.get(&f).copied())
            .filter(|&f| !state[f as usize].is_empty());
        if from.is_none() && fp.hunks.is_empty() {
            continue;
        }
        let id = *ids.entry(fp.path.clone()).or_insert_with(|| {
            paths.push(fp.path);
            state.push(Vec::new());
            (paths.len() - 1) as u32
        });
        if let Some(f) = from
            && !sources.iter().any(|s| s.0 == f)
        {
            sources.push((f, std::mem::take(&mut state[f as usize])));
        }
        plan.push((id, from, fp.hunks));
    }
    for (id, from, hunks) in plan {
        let cur = match from {
            // A path can be the source of only one rename; a second claim gets nothing.
            Some(f) => sources
                .iter_mut()
                .find(|s| s.0 == f)
                .map(|s| std::mem::take(&mut s.1))
                .unwrap_or_default(),
            None => std::mem::take(&mut state[id as usize]),
        };
        // Lines removed by the hunks die in the file they lived in.
        let dead_in = from.unwrap_or(id);
        let mut new = Vec::with_capacity(cur.len());
        let mut pos = 0usize;
        let mut born = 0u32;
        let keep = |new: &mut Vec<u32>, lines: &[u32], o: &mut Out| {
            if let Some(f) = from.filter(|&f| f != id) {
                let first = *o.move_off.last().unwrap_or(&0) as usize;
                for &bc in lines {
                    let len = o.moves.len();
                    match o.moves.last_mut() {
                        Some(m) if len > first && m[0] == f && m[1] == id && m[2] == bc => {
                            m[3] += 1
                        }
                        _ => o.moves.push([f, id, bc, 1]),
                    }
                }
            }
            new.extend_from_slice(lines);
        };
        for (a, b, d) in hunks {
            let start = if b > 0 { a as usize - 1 } else { a as usize };
            if start < pos || start + b as usize > cur.len() {
                o.anomalies += 1;
            }
            let start = start.clamp(pos, cur.len());
            keep(&mut new, &cur[pos..start], o);
            let end = (start + b as usize).min(cur.len());
            // Deaths, run-length encoded by birth commit.
            let first = *o.death_off.last().unwrap_or(&0) as usize; // first death of this commit
            for &bc in &cur[start..end] {
                let len = o.deaths.len();
                match o.deaths.last_mut() {
                    Some(last) if len > first && last[0] == dead_in && last[1] == bc => {
                        last[2] += 1
                    }
                    _ => o.deaths.push([dead_in, bc, 1]),
                }
            }
            pos = end;
            new.extend(std::iter::repeat_n(c, d as usize));
            born += d;
        }
        keep(&mut new, &cur[pos..], o);
        if born > 0 {
            o.births.push([id, born]);
        }
        // A destination that already had lines (rare: git pairs only vanished sources
        // with new paths) would be overwritten; count it so the index refuses to lie.
        if from.is_some() && !state[id as usize].is_empty() {
            o.anomalies += 1;
        }
        state[id as usize] = new;
    }
}

fn write_u32s(p: &Path, v: &[u32]) -> Result<(), String> {
    let mut f = std::io::BufWriter::new(std::fs::File::create(p).map_err(|e| e.to_string())?);
    for x in v {
        f.write_all(&x.to_le_bytes()).map_err(|e| e.to_string())?;
    }
    f.flush().map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hunk_headers() {
        assert_eq!(parse_hunk(b"@@ -3,2 +3,0 @@ fn x"), Some((3, 2, 0)));
        assert_eq!(parse_hunk(b"@@ -0,0 +1,5 @@"), Some((0, 0, 5)));
        assert_eq!(parse_hunk(b"@@ -7 +7 @@"), Some((7, 1, 1)));
        assert_eq!(parse_hunk(b"@@@ -1 +1 @@@"), None);
    }

    #[test]
    fn header_paths() {
        assert_eq!(
            header_path(b"+++ b/a b.txt\t\n").as_deref(),
            Some("a b.txt")
        );
        assert_eq!(header_path(b"--- /dev/null\n"), None);
        assert_eq!(
            header_path(b"+++ \"b/caf\\303\\251\\\"q\"\n").as_deref(),
            Some("café\"q")
        );
    }
}
