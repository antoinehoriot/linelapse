//! Per-file aggregates (live line count, sum of birth times) at any commit, plus live
//! trackers (provenance of a view, territory of a source).
//!
//! State at commit `c` = lines with `birth <= c < death`. Moving forward one commit
//! applies its births and deaths; moving back undoes them. Checkpoints bound the cost of
//! a jump; their spacing is chosen so they fit in `CHECKPOINT_BUDGET` bytes.
//!
//! Trackers use one delta rule (`Key::accumulate`) three ways: per commit while stepping,
//! over the whole prefix for a full rescan (run off the UI thread as a `Job`), and over
//! the commits that went by while that rescan ran (catch-up in `accept`).

use std::fs::File;
use std::path::Path;
use std::sync::Arc;

use memmap2::Mmap;

const CHECKPOINT_BUDGET: usize = 256 << 20;

/// Event streams written by `build.rs` (little-endian u32).
pub struct Events<B: AsRef<[u8]>> {
    /// (file, n) runs, grouped by commit.
    pub births: B,
    pub birth_off: B,
    /// (file, birth commit, n) runs, grouped by death commit.
    pub deaths: B,
    pub death_off: B,
}

// Files are little-endian and mmap is page-aligned, so reinterpreting is valid on the
// little-endian targets we build for. Alignment and length are checked.
fn u32s(b: &[u8]) -> &[u32] {
    assert!(b.len().is_multiple_of(4) && (b.as_ptr() as usize).is_multiple_of(4));
    // SAFETY: length and alignment checked above; u32 has no invalid bit patterns.
    unsafe { std::slice::from_raw_parts(b.as_ptr().cast(), b.len() / 4) }
}

impl<B: AsRef<[u8]>> Events<B> {
    fn births_of(&self, c: usize) -> &[u32] {
        let o = u32s(self.birth_off.as_ref());
        &u32s(self.births.as_ref())[2 * o[c] as usize..2 * o[c + 1] as usize]
    }
    fn deaths_of(&self, c: usize) -> &[u32] {
        let o = u32s(self.death_off.as_ref());
        &u32s(self.deaths.as_ref())[3 * o[c] as usize..3 * o[c + 1] as usize]
    }
}

/// What a tracker counts.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Key {
    /// Live lines in files `lo..hi`, per source. Data length = number of sources.
    Provenance { lo: u32, hi: u32 },
    /// Live lines born from source `src`, per file: counts in `data[..nfiles]`, summed
    /// relative birth times in `data[nfiles..]`, so the source's own mean age is exact.
    Territory { src: u32 },
}

impl Key {
    /// Add (`sign = 1`) or remove (`sign = -1`) commit `c`'s events into `data`.
    fn accumulate<B: AsRef<[u8]>>(
        self,
        ev: &Events<B>,
        src_of: &[u32],
        rel: &[i64],
        c: usize,
        sign: i64,
        data: &mut [i64],
    ) {
        match self {
            Key::Provenance { lo, hi } => {
                for r in ev.births_of(c).chunks_exact(2) {
                    if (lo..hi).contains(&r[0]) {
                        data[src_of[c] as usize] += sign * r[1] as i64;
                    }
                }
                for r in ev.deaths_of(c).chunks_exact(3) {
                    if (lo..hi).contains(&r[0]) {
                        data[src_of[r[1] as usize] as usize] -= sign * r[2] as i64;
                    }
                }
            }
            Key::Territory { src } => {
                let nf = data.len() / 2;
                if src_of[c] == src {
                    for r in ev.births_of(c).chunks_exact(2) {
                        let (f, k) = (r[0] as usize, sign * r[1] as i64);
                        data[f] += k;
                        data[nf + f] += k * rel[c];
                    }
                }
                for r in ev.deaths_of(c).chunks_exact(3) {
                    let bc = r[1] as usize;
                    if src_of[bc] == src {
                        let (f, k) = (r[0] as usize, sign * r[2] as i64);
                        data[f] -= k;
                        data[nf + f] -= k * rel[bc];
                    }
                }
            }
        }
    }
}

/// A full rescan to run off the UI thread.
pub struct Job<B: AsRef<[u8]>> {
    ev: Arc<Events<B>>,
    src_of: Arc<Vec<u32>>,
    rel: Arc<Vec<i64>>,
    key: Key,
    at: usize,
    len: usize,
}

/// Result of a `Job`, to hand back to `Engine::accept`.
pub struct Done {
    key: Key,
    at: usize,
    data: Vec<i64>,
}

impl<B: AsRef<[u8]>> Job<B> {
    pub fn run(self) -> Done {
        let mut data = vec![0i64; self.len];
        for c in 0..=self.at {
            self.key
                .accumulate(&self.ev, &self.src_of, &self.rel, c, 1, &mut data);
        }
        Done {
            key: self.key,
            at: self.at,
            data,
        }
    }
}

struct Tracker {
    key: Option<Key>,
    data: Vec<i64>,
    valid: bool,
    pending: bool,
}

impl Tracker {
    const fn new() -> Self {
        Tracker {
            key: None,
            data: Vec::new(),
            valid: false,
            pending: false,
        }
    }
    fn set(&mut self, key: Option<Key>) {
        if self.key != key {
            self.key = key;
            self.valid = false;
            // A pending job for the old key is discarded in `accept`; allow a new one.
            self.pending = false;
        }
    }
}

#[derive(Clone)]
struct Snapshot {
    count: Vec<i64>,
    /// Sum over live lines of (birth time - times[0]), in seconds.
    sum: Vec<i64>,
}

pub struct Engine<B: AsRef<[u8]>> {
    ev: Arc<Events<B>>,
    pub times: Vec<i64>,
    nfiles: usize,
    stride: usize,
    checkpoints: Vec<Snapshot>,
    /// Total live lines after each commit.
    pub totals: Vec<i64>,
    /// (lines born, lines dead) in each commit.
    pub churn: Vec<(u64, u64)>,
    /// Calendar year of each cohort (lines grouped by the year they were born).
    pub cohort_years: Vec<i32>,
    /// Live lines per cohort after each commit, flattened: row `c` is
    /// `cohorts[c * ny..(c + 1) * ny]`.
    cohorts: Vec<i64>,
    cur: usize,
    state: Snapshot,
    src_of: Arc<Vec<u32>>,
    /// Commit times relative to the first commit (what `Snapshot::sum` adds up).
    rel: Arc<Vec<i64>>,
    nsrc: usize,
    trackers: [Tracker; 2], // 0 = provenance, 1 = territory
}

/// Calendar year (UTC) of a unix timestamp (civil-from-days, Howard Hinnant).
pub fn year_of(unix: i64) -> i32 {
    let z = unix.div_euclid(86400) + 719468;
    let era = z.div_euclid(146097);
    let doe = z - era * 146097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    (yoe + era * 400 + i64::from(m <= 2)) as i32
}

impl Engine<Mmap> {
    pub fn open(dir: &Path, times: Vec<i64>, nfiles: usize) -> std::io::Result<Self> {
        let map = |n: &str| -> std::io::Result<Mmap> {
            let f = File::open(dir.join(n))?;
            // SAFETY: read-only map of cache files nobody mutates while we run.
            unsafe { Mmap::map(&f) }
        };
        let ev = Events {
            births: map("births.u32")?,
            birth_off: map("birth_off.u32")?,
            deaths: map("deaths.u32")?,
            death_off: map("death_off.u32")?,
        };
        Ok(Engine::new(ev, times, nfiles))
    }
}

/// Aggregate of a set of files: line count and summed relative birth time.
#[derive(Clone, Copy, Default, Debug, PartialEq)]
pub struct Agg {
    pub count: i64,
    pub sum: i64,
}

impl<B: AsRef<[u8]>> Engine<B> {
    pub fn new(ev: Events<B>, times: Vec<i64>, nfiles: usize) -> Self {
        let n = times.len();
        assert!(n > 0);
        assert_eq!(u32s(ev.birth_off.as_ref()).len(), n + 1);
        assert_eq!(u32s(ev.death_off.as_ref()).len(), n + 1);
        let max_ck = (CHECKPOINT_BUDGET / (nfiles.max(1) * 16)).max(2);
        let stride = n.div_ceil(max_ck).max(64);
        let rel: Vec<i64> = times.iter().map(|t| t - times[0]).collect();
        let mut e = Engine {
            ev: Arc::new(ev),
            times,
            nfiles,
            stride,
            checkpoints: Vec::new(),
            totals: Vec::with_capacity(n),
            churn: Vec::with_capacity(n),
            cohort_years: Vec::new(),
            cohorts: Vec::new(),
            cur: 0,
            state: Snapshot {
                count: vec![0; nfiles],
                sum: vec![0; nfiles],
            },
            src_of: Arc::new(vec![0; n]),
            rel: Arc::new(rel),
            nsrc: 1,
            trackers: [Tracker::new(), Tracker::new()],
        };
        let mut total = 0i64;
        for c in 0..n {
            let (b, d) = e.apply(c, 1);
            total += b as i64 - d as i64;
            e.totals.push(total);
            e.churn.push((b, d));
            if c % stride == 0 {
                e.checkpoints.push(e.state.clone());
            }
        }
        e.cur = n - 1;
        e.build_cohorts();
        e
    }

    /// Live lines per birth-year cohort after commit `c` (oldest cohort first).
    pub fn cohort_row(&self, c: usize) -> &[i64] {
        let ny = self.cohort_years.len();
        &self.cohorts[c * ny..(c + 1) * ny]
    }

    /// One pass over all events: running live-line count per birth year.
    fn build_cohorts(&mut self) {
        let year: Vec<i32> = self.times.iter().map(|&t| year_of(t)).collect();
        let first = year.iter().copied().min().unwrap_or(1970);
        let last = year.iter().copied().max().unwrap_or(first);
        let ny = (last - first + 1) as usize;
        self.cohort_years = (first..=last).collect();
        let n = self.len();
        let mut row = vec![0i64; ny];
        self.cohorts = Vec::with_capacity(n * ny);
        for c in 0..n {
            for r in self.ev.births_of(c).chunks_exact(2) {
                row[(year[c] - first) as usize] += r[1] as i64;
            }
            for r in self.ev.deaths_of(c).chunks_exact(3) {
                row[(year[r[1] as usize] - first) as usize] -= r[2] as i64;
            }
            self.cohorts.extend_from_slice(&row);
        }
    }

    pub fn len(&self) -> usize {
        self.times.len()
    }

    pub fn current(&self) -> usize {
        self.cur
    }

    fn rel(&self, c: usize) -> i64 {
        self.rel[c]
    }

    /// Apply (`sign = 1`) or undo (`sign = -1`) commit `c`. Returns (born, dead) line counts.
    fn apply(&mut self, c: usize, sign: i64) -> (u64, u64) {
        let ev = Arc::clone(&self.ev);
        let t = self.rel(c);
        let (mut born, mut dead) = (0u64, 0u64);
        for r in ev.births_of(c).chunks_exact(2) {
            let (f, k) = (r[0] as usize, r[1] as i64);
            self.state.count[f] += sign * k;
            self.state.sum[f] += sign * k * t;
            born += k as u64;
        }
        for r in ev.deaths_of(c).chunks_exact(3) {
            let (f, bc, k) = (r[0] as usize, r[1] as usize, r[2] as i64);
            self.state.count[f] -= sign * k;
            self.state.sum[f] -= sign * k * self.rel(bc);
            dead += k as u64;
        }
        for tr in &mut self.trackers {
            if let (Some(key), true) = (tr.key, tr.valid) {
                key.accumulate(&ev, &self.src_of, &self.rel, c, sign, &mut tr.data);
            }
        }
        (born, dead)
    }

    /// Move to commit `t`, stepping or via the nearest checkpoint, whichever is shorter.
    pub fn seek(&mut self, t: usize) {
        let t = t.min(self.len() - 1);
        let k = t / self.stride;
        let from_ck = t - k * self.stride;
        // A restore invalidates the trackers (a full rescan), so while any is active we
        // keep stepping unless the jump is much longer than a stride.
        let watching = self.trackers.iter().any(|tr| tr.key.is_some());
        let restore_cost = if watching { 2 * self.stride } else { 0 };
        if self.cur.abs_diff(t) > from_ck + restore_cost {
            for tr in &mut self.trackers {
                tr.valid = false;
            }
            self.state.clone_from(&self.checkpoints[k]);
            self.cur = k * self.stride;
        }
        while self.cur < t {
            self.cur += 1;
            self.apply(self.cur, 1);
        }
        while self.cur > t {
            self.apply(self.cur, -1);
            self.cur -= 1;
        }
    }

    /// Aggregate over the contiguous file id range `lo..hi` at the current commit.
    pub fn agg(&self, lo: usize, hi: usize) -> Agg {
        Agg {
            count: self.state.count[lo..hi].iter().sum(),
            sum: self.state.sum[lo..hi].iter().sum(),
        }
    }

    /// Mean age in years of an aggregate at the current commit.
    pub fn mean_age_years(&self, a: Agg) -> Option<f64> {
        (a.count > 0).then(|| {
            let mean = a.sum as f64 / a.count as f64;
            (self.rel(self.cur) as f64 - mean) / (365.25 * 86400.0)
        })
    }

    /// Per-file line count at each checkpoint (for stable ordering by peak size).
    pub fn checkpoint_counts(&self) -> impl Iterator<Item = &[i64]> {
        self.checkpoints.iter().map(|s| s.count.as_slice())
    }

    /// Source of each commit (index into the caller's source names).
    pub fn set_sources(&mut self, src_of: Vec<u32>) {
        assert_eq!(src_of.len(), self.len());
        self.nsrc = src_of.iter().max().map_or(0, |&m| m as usize + 1);
        self.src_of = Arc::new(src_of);
        for tr in &mut self.trackers {
            tr.valid = false;
        }
    }

    /// Follow the provenance of files `lo..hi`.
    pub fn watch_provenance(&mut self, lo: u32, hi: u32) {
        self.trackers[0].set(Some(Key::Provenance { lo, hi }));
    }

    /// Follow the territory of a source, or stop.
    pub fn watch_territory(&mut self, src: Option<u32>) {
        self.trackers[1].set(src.map(|src| Key::Territory { src }));
    }

    /// Rescans to run (off the UI thread) for trackers that are not up to date.
    pub fn take_jobs(&mut self) -> Vec<Job<B>> {
        let mut jobs = Vec::new();
        for tr in &mut self.trackers {
            if let (Some(key), false, false) = (tr.key, tr.valid, tr.pending) {
                tr.pending = true;
                let len = match key {
                    Key::Provenance { .. } => self.nsrc,
                    Key::Territory { .. } => 2 * self.nfiles,
                };
                jobs.push(Job {
                    ev: Arc::clone(&self.ev),
                    src_of: Arc::clone(&self.src_of),
                    rel: Arc::clone(&self.rel),
                    key,
                    at: self.cur,
                    len,
                });
            }
        }
        jobs
    }

    /// Take a finished rescan, catch it up to the current commit, and make it live.
    /// Results for a key that is no longer watched are dropped.
    pub fn accept(&mut self, mut done: Done) {
        let ev = Arc::clone(&self.ev);
        let src_of = Arc::clone(&self.src_of);
        let rel = Arc::clone(&self.rel);
        let cur = self.cur;
        let Some(tr) = self.trackers.iter_mut().find(|t| t.key == Some(done.key)) else {
            return;
        };
        if tr.valid {
            return;
        }
        for c in done.at + 1..=cur {
            done.key
                .accumulate(&ev, &src_of, &rel, c, 1, &mut done.data);
        }
        for c in (cur + 1..=done.at).rev() {
            done.key
                .accumulate(&ev, &src_of, &rel, c, -1, &mut done.data);
        }
        tr.data = done.data;
        tr.valid = true;
        tr.pending = false;
    }

    /// True while any watched tracker waits for a rescan.
    pub fn busy(&self) -> bool {
        self.trackers.iter().any(|t| t.key.is_some() && !t.valid)
    }

    /// Live lines per source in the watched view, if up to date.
    pub fn provenance(&self) -> Option<&[i64]> {
        let t = &self.trackers[0];
        t.valid.then_some(t.data.as_slice())
    }

    /// Live lines of the watched source in files `lo..hi` (count and birth-time sum, so
    /// `mean_age_years` gives the source's own age there), if up to date.
    pub fn territory(&self, lo: usize, hi: usize) -> Option<Agg> {
        let t = &self.trackers[1];
        let nf = self.nfiles;
        (t.valid && t.key.is_some()).then(|| Agg {
            count: t.data[lo..hi].iter().sum(),
            sum: t.data[nf + lo..nf + hi].iter().sum(),
        })
    }

    /// Run pending rescans on this thread (tests).
    #[cfg(test)]
    pub fn sync(&mut self) {
        for j in self.take_jobs() {
            let d = j.run();
            self.accept(d);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn le32(v: &[u32]) -> Vec<u8> {
        v.iter().flat_map(|x| x.to_le_bytes()).collect()
    }

    /// Synthetic history of single lines (file, birth, death); death = u32::MAX if alive.
    fn build(lines: &[(u32, u32, u32)], n: usize, nf: usize) -> Engine<Vec<u8>> {
        let (mut births, mut boff) = (Vec::new(), vec![0u32]);
        let (mut deaths, mut doff) = (Vec::new(), vec![0u32]);
        for c in 0..n as u32 {
            for f in 0..nf as u32 {
                let k = lines.iter().filter(|l| l.0 == f && l.1 == c).count() as u32;
                if k > 0 {
                    births.extend([f, k]);
                }
                for b in 0..=c {
                    let k = lines
                        .iter()
                        .filter(|l| l.0 == f && l.1 == b && l.2 == c)
                        .count() as u32;
                    if k > 0 {
                        deaths.extend([f, b, k]);
                    }
                }
            }
            boff.push(births.len() as u32 / 2);
            doff.push(deaths.len() as u32 / 3);
        }
        let ev = Events {
            births: le32(&births),
            birth_off: le32(&boff),
            deaths: le32(&deaths),
            death_off: le32(&doff),
        };
        let times = (0..n as i64).map(|c| 1_000_000 + c * 86400).collect();
        Engine::new(ev, times, nf)
    }

    fn random_lines(
        seed: u64,
        count: usize,
        n: usize,
        nf: usize,
        life: u32,
    ) -> Vec<(u32, u32, u32)> {
        let mut x = seed;
        (0..count)
            .map(|_| {
                x = x
                    .wrapping_mul(6364136223846793005)
                    .wrapping_add(1442695040888963407);
                let b = (x >> 33) as u32 % n as u32;
                let l = (x >> 13) as u32 % life;
                let d = if b + l + 1 >= n as u32 {
                    u32::MAX
                } else {
                    b + l + 1
                };
                ((x >> 50) as u32 % nf as u32, b, d)
            })
            .collect()
    }

    fn alive(lines: &[(u32, u32, u32)], t: usize) -> impl Iterator<Item = &(u32, u32, u32)> {
        lines
            .iter()
            .filter(move |l| l.1 as usize <= t && (l.2 as usize) > t)
    }

    #[test]
    fn seek_and_agg_match_brute_force() {
        let (n, nf) = (300, 6);
        let lines = random_lines(7, 3000, n, nf, 400);
        let mut e = build(&lines, n, nf);
        for &t in &[
            0, 299, 1, 150, 151, 149, 64, 63, 65, 250, 5, 298, 128, 0, 299,
        ] {
            e.seek(t);
            assert_eq!(e.current(), t);
            assert_eq!(e.totals[t], alive(&lines, t).count() as i64);
            for (lo, hi) in [(0, 6), (1, 3), (5, 6), (2, 2)] {
                let al: Vec<_> = alive(&lines, t)
                    .filter(|l| (lo..hi).contains(&l.0))
                    .collect();
                let a = e.agg(lo as usize, hi as usize);
                assert_eq!(a.count, al.len() as i64, "t={t} {lo}..{hi}");
                if let Some(age) = e.mean_age_years(a) {
                    let mb = al.iter().map(|l| l.1 as f64).sum::<f64>() / al.len() as f64;
                    assert!((age - (t as f64 - mb) / 365.25).abs() < 1e-6);
                }
            }
        }
    }

    /// Both trackers stay exact through playback, jumps, backwards play, and through
    /// rescans that finish after the playhead moved on (background jobs).
    #[test]
    fn trackers_follow_steps_jumps_and_late_jobs() {
        let (n, nf) = (900, 5);
        let lines = random_lines(99, 4000, n, nf, 600);
        let mut e = build(&lines, n, nf);
        let src: Vec<u32> = (0..n as u32).map(|c| (c * 7) % 5).collect();
        e.set_sources(src.clone());
        e.seek(0);
        let (lo, hi) = (1u32, 4u32);
        let sel = 2u32;
        e.watch_provenance(lo, hi);
        e.watch_territory(Some(sel));
        let mut path: Vec<usize> = (0..n).step_by(13).collect();
        path.extend([899, 3, 450, 451, 449, 10, 880]);
        path.extend((0..n).rev().step_by(29));
        let mut late: Vec<Job<Vec<u8>>> = Vec::new();
        for (i, &t) in path.iter().enumerate() {
            e.seek(t);
            // Every third step, finish jobs one step late, as a background thread would.
            if i % 3 == 0 {
                late.extend(e.take_jobs());
            } else {
                for j in late.drain(..) {
                    let d = j.run();
                    e.accept(d);
                }
                e.sync();
            }
            if e.busy() {
                continue;
            }
            let prov = e.provenance().expect("provenance");
            for s in 0..5u32 {
                let want = alive(&lines, t)
                    .filter(|l| (lo..hi).contains(&l.0) && src[l.1 as usize] == s)
                    .count() as i64;
                assert_eq!(prov[s as usize], want, "prov t={t} s={s}");
            }
            for (flo, fhi) in [(0usize, 5usize), (2, 3), (0, 1)] {
                let mine: Vec<_> = alive(&lines, t)
                    .filter(|l| (flo as u32..fhi as u32).contains(&l.0) && src[l.1 as usize] == sel)
                    .collect();
                let got = e.territory(flo, fhi).expect("territory");
                assert_eq!(got.count, mine.len() as i64, "terr t={t} {flo}..{fhi}");
                // Birth-time sum, in the engine's relative seconds (one day per commit).
                let want_sum: i64 = mine.iter().map(|l| l.1 as i64 * 86400).sum();
                assert_eq!(got.sum, want_sum, "terr age t={t} {flo}..{fhi}");
            }
        }
        // Switching keys invalidates, and stale results for the old key are dropped.
        let stale = e.take_jobs();
        e.watch_territory(Some(3));
        for j in stale {
            let d = j.run();
            e.accept(d);
        }
        e.sync();
        let t = e.current();
        let want = alive(&lines, t).filter(|l| src[l.1 as usize] == 3).count() as i64;
        assert_eq!(e.territory(0, nf).map(|a| a.count), Some(want));
        e.watch_territory(None);
        assert_eq!(e.territory(0, nf), None);
    }

    #[test]
    fn cohorts_match_brute_force() {
        let (n, nf) = (400, 3);
        let lines = random_lines(5, 2500, n, nf, 300);
        // One commit per day from 1,000,000 s: spans 1970 and 1971.
        let e = build(&lines, n, nf);
        let years: Vec<i32> = e.times.iter().map(|&t| year_of(t)).collect();
        assert_eq!(e.cohort_years, vec![1970, 1971]);
        for t in [0, 1, 199, 300, 399] {
            let row = e.cohort_row(t);
            assert_eq!(row.iter().sum::<i64>(), e.totals[t]);
            for (k, &y) in e.cohort_years.iter().enumerate() {
                let want = alive(&lines, t)
                    .filter(|l| years[l.1 as usize] == y)
                    .count() as i64;
                assert_eq!(row[k], want, "t={t} y={y}");
            }
        }
        assert_eq!(year_of(0), 1970);
        assert_eq!(year_of(1_794_088_322), 2026);
        assert_eq!(year_of(951_782_400), 2000); // 2000-02-29
    }
}
