//! Co-change: which parts of the repository change in the same commits as a given one.
//!
//! Shown in the panel while a tile is hovered: the units of the current view (outside the
//! hovered tile's own folder) that its commits also touch, and when.
//!
//! Decisions (why, so they are not relitigated):
//! - Only commits touching `MIN_FILES..=MAX_FILES` files count. A first-parent merge or a
//!   tree-wide cleanup touches hundreds of files at once and would couple everything with
//!   everything (same idea as code-maat's `--max-changeset-size`, default 30).
//! - A partner must pass two bars (`Bars::PANEL`): 10% of the hovered unit's commits, and a lift
//!   of 10 (it changes 10 times more often with the hovered unit than in
//!   an average commit). The lift drops hubs that change with everything (`include/linux`
//!   is in 31% of Linux commits) without a repository-specific list. Checked on Linux:
//!   it keeps fs/ceph -> net/ceph, drivers/block; net/sunrpc -> fs/nfs, fs/nfsd, fs/lockd;
//!   drivers/s390 -> arch/s390, and drops include/linux, MAINTAINERS, arch/x86.
//! - 5 shared commits at least, so a unit with 3 commits does not get 33% partners.
//! - Units come from the tree, not from what is drawn (`app::view_units`): the treemap's
//!   two levels inside the view, and outside it each branch at the finest level that does
//!   not contain the view, so a zoomed view still finds partners elsewhere (zoomed on
//!   fs/ceph, every partner is outside: net/ceph, drivers/block). Tiles appearing or
//!   vanishing during playback do not change the answer, so it is cached per view.
//! - Whole history, not up to the playhead: the strip of shared commits over time shows
//!   when the coupling happened, and the panel marks the playhead on it.

use std::collections::HashMap;

pub const MIN_FILES: usize = 2;
pub const MAX_FILES: usize = 40;
pub const MAX_ROWS: usize = 5;

/// What a partner must reach. `Bars::PANEL` is what the window shows.
#[derive(Clone, Copy, Debug)]
pub struct Bars {
    pub min_share: f64,
    pub min_lift: f64,
    pub min_shared: u32,
}

impl Bars {
    pub const PANEL: Bars = Bars {
        min_share: 0.10,
        min_lift: 10.0,
        min_shared: 5,
    };
}
/// Time buckets of the strip, over commit positions (same axis as the timeline).
pub const BINS: usize = 60;

/// Kept commits and the files they touched, in both directions (compressed rows).
pub struct CoChange {
    ncommits: usize,
    /// Commit position of each kept commit.
    commit_at: Vec<u32>,
    files_off: Vec<u32>,
    files: Vec<u32>,
    /// Kept commits touching each file.
    commits_off: Vec<u32>,
    commits: Vec<u32>,
}

/// A view's units: `unit_of[file]` = index into `nodes`, or `u32::MAX` outside the view.
pub struct Units {
    pub nodes: Vec<usize>,
    pub parent: Vec<usize>,
    range: Vec<(u32, u32)>,
    unit_of: Vec<u32>,
    /// Kept commits touching each unit.
    total: Vec<u32>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Partner {
    /// Tree node of the partner unit.
    pub node: usize,
    pub shared: u32,
    /// Share of the hovered unit's commits that also touch the partner.
    pub share: f64,
    /// Shared commits per time bucket.
    pub bins: Vec<u32>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Partners {
    /// Kept commits touching the hovered unit.
    pub commits: u32,
    pub rows: Vec<Partner>,
}

impl CoChange {
    /// `touched(c, out)` fills `out` with the sorted, deduplicated files of commit `c`.
    pub fn build(
        ncommits: usize,
        nfiles: usize,
        mut touched: impl FnMut(usize, &mut Vec<u32>),
    ) -> Self {
        let mut buf = Vec::new();
        let (mut commit_at, mut files_off, mut files) = (Vec::new(), vec![0u32], Vec::new());
        let mut per_file = vec![0u32; nfiles + 1];
        for c in 0..ncommits {
            touched(c, &mut buf);
            if !(MIN_FILES..=MAX_FILES).contains(&buf.len()) {
                continue;
            }
            commit_at.push(c as u32);
            for &f in &buf {
                per_file[f as usize + 1] += 1;
            }
            files.extend_from_slice(&buf);
            files_off.push(files.len() as u32);
        }
        for i in 0..nfiles {
            per_file[i + 1] += per_file[i];
        }
        let commits_off = per_file.clone();
        let mut fill = per_file;
        let mut commits = vec![0u32; files.len()];
        for k in 0..commit_at.len() {
            for &f in &files[files_off[k] as usize..files_off[k + 1] as usize] {
                commits[fill[f as usize] as usize] = k as u32;
                fill[f as usize] += 1;
            }
        }
        CoChange {
            ncommits,
            commit_at,
            files_off,
            files,
            commits_off,
            commits,
        }
    }

    fn files_of(&self, k: usize) -> &[u32] {
        &self.files[self.files_off[k] as usize..self.files_off[k + 1] as usize]
    }

    /// Units of a view, given as (node, parent node, file range) in any order.
    pub fn units(&self, nfiles: usize, list: &[(usize, usize, u32, u32)]) -> Units {
        let mut unit_of = vec![u32::MAX; nfiles];
        for (i, &(_, _, lo, hi)) in list.iter().enumerate() {
            unit_of[lo as usize..hi as usize].fill(i as u32);
        }
        let mut total = vec![0u32; list.len()];
        let mut seen: Vec<u32> = Vec::new();
        for k in 0..self.commit_at.len() {
            seen.clear();
            seen.extend(self.files_of(k).iter().map(|&f| unit_of[f as usize]));
            seen.retain(|&u| u != u32::MAX);
            seen.sort_unstable();
            seen.dedup();
            for &u in &seen {
                total[u as usize] += 1;
            }
        }
        Units {
            nodes: list.iter().map(|l| l.0).collect(),
            parent: list.iter().map(|l| l.1).collect(),
            range: list.iter().map(|l| (l.2, l.3)).collect(),
            unit_of,
            total,
        }
    }

    /// Partners of unit `u`: units outside its parent that pass the share, lift and count
    /// bars, strongest first, at most `MAX_ROWS`.
    pub fn partners(&self, units: &Units, u: usize, bars: Bars) -> Partners {
        // Kept commits touching u, without duplicates (a unit spans many files).
        let mut mine: Vec<u32> = Vec::new();
        let (lo, hi) = units.range[u];
        for f in lo as usize..hi as usize {
            mine.extend_from_slice(
                &self.commits[self.commits_off[f] as usize..self.commits_off[f + 1] as usize],
            );
        }
        mine.sort_unstable();
        mine.dedup();
        let n_mine = mine.len() as u32;
        let kept = self.commit_at.len().max(1) as f64;
        let mut shared: HashMap<u32, Vec<u32>> = HashMap::new();
        let mut seen: Vec<u32> = Vec::new();
        for &k in &mine {
            seen.clear();
            seen.extend(
                self.files_of(k as usize)
                    .iter()
                    .map(|&f| units.unit_of[f as usize]),
            );
            seen.retain(|&v| v != u32::MAX && units.parent[v as usize] != units.parent[u]);
            seen.sort_unstable();
            seen.dedup();
            for &v in &seen {
                shared.entry(v).or_default().push(k);
            }
        }
        let mut rows: Vec<Partner> = shared
            .into_iter()
            .filter_map(|(v, ks)| {
                let n = ks.len() as u32;
                let share = n as f64 / n_mine.max(1) as f64;
                let base = units.total[v as usize] as f64 / kept;
                let lift = share / base.max(f64::MIN_POSITIVE);
                (n >= bars.min_shared && share >= bars.min_share && lift >= bars.min_lift).then(
                    || {
                        let mut bins = vec![0u32; BINS];
                        for k in ks {
                            let c = self.commit_at[k as usize] as usize;
                            bins[(c * BINS / self.ncommits.max(1)).min(BINS - 1)] += 1;
                        }
                        Partner {
                            node: units.nodes[v as usize],
                            shared: n,
                            share,
                            bins,
                        }
                    },
                )
            })
            .collect();
        rows.sort_by(|a, b| b.share.total_cmp(&a.share).then(a.node.cmp(&b.node)));
        rows.truncate(MAX_ROWS);
        Partners {
            commits: n_mine,
            rows,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Brute force over explicit commit file lists, same rules as `partners`.
    fn brute(
        commits: &[Vec<u32>],
        list: &[(usize, usize, u32, u32)],
        u: usize,
        bars: Bars,
    ) -> (u32, Vec<(usize, u32)>) {
        let unit = |f: u32| list.iter().position(|l| (l.2..l.3).contains(&f));
        let kept: Vec<&Vec<u32>> = commits
            .iter()
            .filter(|c| (MIN_FILES..=MAX_FILES).contains(&c.len()))
            .collect();
        let units_of = |c: &Vec<u32>| {
            let mut v: Vec<usize> = c.iter().filter_map(|&f| unit(f)).collect();
            v.sort();
            v.dedup();
            v
        };
        let mine: Vec<&&Vec<u32>> = kept.iter().filter(|c| units_of(c).contains(&u)).collect();
        let mut out = Vec::new();
        for v in 0..list.len() {
            if list[v].1 == list[u].1 {
                continue;
            }
            let n = mine.iter().filter(|c| units_of(c).contains(&v)).count() as u32;
            let total = kept.iter().filter(|c| units_of(c).contains(&v)).count() as f64;
            let share = n as f64 / mine.len().max(1) as f64;
            let lift = share / (total / kept.len() as f64).max(f64::MIN_POSITIVE);
            if n >= bars.min_shared && share >= bars.min_share && lift >= bars.min_lift {
                out.push((list[v].0, n));
            }
        }
        out.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
        out.truncate(MAX_ROWS);
        (mine.len() as u32, out)
    }

    fn cochange(commits: &[Vec<u32>], nfiles: usize) -> CoChange {
        CoChange::build(commits.len(), nfiles, |c, out| {
            out.clear();
            out.extend_from_slice(&commits[c]);
            out.sort_unstable();
            out.dedup();
        })
    }

    /// Hand-made case: a pair across folders is found, a hub that changes with everything
    /// is dropped by the lift, a sibling is ignored, big commits do not count.
    #[test]
    fn finds_partner_drops_hub_and_siblings() {
        // Files: 0-1 unit A (parent 100), 2 unit B (parent 100, A's sibling),
        // 3 unit C (parent 200), 4 unit HUB (parent 300), 5-49 unit D (parent 400).
        let list = [
            (10, 100, 0, 2),
            (11, 100, 2, 3),
            (12, 200, 3, 4),
            (13, 300, 4, 5),
            (14, 400, 5, 50),
        ];
        let mut commits = Vec::new();
        for _ in 0..8 {
            commits.push(vec![0, 3, 4]); // A + C + hub
            commits.push(vec![1, 2]); // A + sibling B
        }
        for i in 0..200u32 {
            commits.push(vec![4, 5 + i % 45]); // hub with D: hub is in most commits
        }
        commits.push((0..45).collect()); // 45 files: too big, ignored
        let cc = cochange(&commits, 50);
        let units = cc.units(50, &list);
        let p = cc.partners(&units, 0, Bars::PANEL);
        assert_eq!(p.commits, 16);
        assert_eq!(p.rows.len(), 1, "{p:?}");
        assert_eq!((p.rows[0].node, p.rows[0].shared), (12, 8));
        assert_eq!(p.rows[0].bins.iter().sum::<u32>(), 8);
        assert_eq!(brute(&commits, &list, 0, Bars::PANEL), (16, vec![(12, 8)]));
    }

    #[test]
    fn matches_brute_force_on_random_histories() {
        let mut x = 7u64;
        let mut rnd = |m: u32| {
            x = x
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            (x >> 33) as u32 % m
        };
        let mut found = 0;
        for _ in 0..20 {
            let nfiles = 60u32;
            // 12 units of 5 files under 4 parents; files clustered so pairs exist.
            let list: Vec<(usize, usize, u32, u32)> = (0..12)
                .map(|i| (1000 + i, i % 4, i as u32 * 5, i as u32 * 5 + 5))
                .collect();
            let commits: Vec<Vec<u32>> = (0..400)
                .map(|_| {
                    let k = 1 + rnd(5) as usize;
                    let base = rnd(nfiles);
                    let mut c: Vec<u32> = (0..k).map(|_| (base + rnd(12)) % nfiles).collect();
                    if rnd(30) == 0 {
                        c.extend((0..50).map(|_| rnd(nfiles))); // a big one
                    }
                    c.sort();
                    c.dedup();
                    c
                })
                .collect();
            let cc = cochange(&commits, nfiles as usize);
            let units = cc.units(nfiles as usize, &list);
            // The panel's bars, and lower ones so that many partners pass.
            let low = Bars {
                min_share: 0.05,
                min_lift: 1.2,
                min_shared: 2,
            };
            for bars in [Bars::PANEL, low] {
                for u in 0..list.len() {
                    let p = cc.partners(&units, u, bars);
                    let got: Vec<(usize, u32)> =
                        p.rows.iter().map(|r| (r.node, r.shared)).collect();
                    assert_eq!(
                        (p.commits, got),
                        brute(&commits, &list, u, bars),
                        "unit {u}"
                    );
                    found += p.rows.len();
                    for r in &p.rows {
                        assert_eq!(r.bins.iter().sum::<u32>(), r.shared);
                    }
                }
            }
        }
        assert!(
            found > 100,
            "the test must exercise partners, found {found}"
        );
    }
}
