//! Where lines came from (provenance) and which commits deserve a marker (moments).

use crate::build::CommitMeta;

/// Who brought a mainline commit in.
/// - merge of a remote tree ("Merge tag 'x' of git://host/.../netdev/net") -> "netdev/net"
/// - GitHub merge ("Merge pull request #12 from alice/feature") -> "alice"
/// - local branch merge ("Merge branch 'fix'") -> "branch fix"
/// - plain commit on the mainline -> its author
pub fn source_of(c: &CommitMeta) -> String {
    let s = c.subject.as_str();
    if !c.merge {
        return c.author.clone();
    }
    // Any URL-like token names the tree: "Merge tag 'x' of git://host/.../netdev/net",
    // and the older "Merge git://host/.../davem/net-2.6" or "Merge host:/path/rmk/linux-arm".
    if let Some(url) = s
        .split_whitespace()
        .skip(1)
        .find(|t| t.contains("://") || t.contains(":/") || t.starts_with("git@"))
    {
        let url = url
            .trim_end_matches(['/', ':', ','])
            .trim_end_matches(".git");
        let parts: Vec<&str> = url.split(['/', ':']).filter(|p| !p.is_empty()).collect();
        if parts.len() >= 2 {
            return format!("{}/{}", parts[parts.len() - 2], parts[parts.len() - 1]);
        }
    }
    if let Some(rest) = s.strip_prefix("Merge pull request #")
        && let Some(i) = rest.find(" from ")
    {
        let who = rest[i + 6..].split('/').next().unwrap_or("");
        if !who.is_empty() {
            return who.to_string();
        }
    }
    if let Some(rest) = s.strip_prefix("Merge branch '")
        && let Some(name) = rest.split('\'').next()
    {
        return format!("branch {name}");
    }
    format!("merge by {}", c.author)
}

/// Intern sources: returns (names, source index per commit).
pub fn sources(commits: &[CommitMeta]) -> (Vec<String>, Vec<u32>) {
    let mut names = Vec::new();
    let mut idx = std::collections::HashMap::new();
    let per = commits
        .iter()
        .enumerate()
        .map(|(i, c)| {
            // The root commit is usually an import of pre-git history (Linux: v2.6.12-rc2),
            // not code its author wrote: label it as such.
            let s = if i == 0 {
                "initial import".to_string()
            } else {
                source_of(c)
            };
            *idx.entry(s.clone()).or_insert_with(|| {
                names.push(s);
                (names.len() - 1) as u32
            })
        })
        .collect();
    (names, per)
}

pub struct Moment {
    pub commit: usize,
    pub born: u64,
    pub dead: u64,
}

/// The `k` commits with the most churn, at least `n / (3k)` commits apart, in time order.
/// The first commit is skipped: it is the initial import, already the left edge.
pub fn moments(churn: &[(u64, u64)], k: usize) -> Vec<Moment> {
    let n = churn.len();
    let gap = (n / (3 * k.max(1))).max(1);
    let mut idx: Vec<usize> = (1..n).filter(|&c| churn[c].0 + churn[c].1 > 0).collect();
    idx.sort_by_key(|&c| std::cmp::Reverse(churn[c].0 + churn[c].1));
    let mut picked: Vec<usize> = Vec::new();
    for c in idx {
        if picked.len() == k {
            break;
        }
        if picked.iter().all(|&p| p.abs_diff(c) >= gap) {
            picked.push(c);
        }
    }
    picked.sort();
    picked
        .into_iter()
        .map(|c| Moment {
            commit: c,
            born: churn[c].0,
            dead: churn[c].1,
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn c(merge: bool, author: &str, subject: &str) -> CommitMeta {
        CommitMeta {
            hash: String::new(),
            time: 0,
            merge,
            author: author.into(),
            subject: subject.into(),
        }
    }

    #[test]
    fn sources_from_subjects() {
        let k = "Merge tag 'net-6.1' of git://git.kernel.org/pub/scm/linux/kernel/git/netdev/net";
        assert_eq!(source_of(&c(true, "Linus", k)), "netdev/net");
        let b = "Merge branch 'akpm' (patches from Andrew)";
        assert_eq!(source_of(&c(true, "Linus", b)), "branch akpm");
        let gh = "Merge pull request #42 from alice/feature-x";
        assert_eq!(source_of(&c(true, "bob", gh)), "alice");
        let url = "Merge branch 'for-linus' of https://github.com/foo/bar.git";
        assert_eq!(source_of(&c(true, "Linus", url)), "foo/bar");
        assert_eq!(source_of(&c(false, "Ada", "fix: x")), "Ada");
        assert_eq!(source_of(&c(true, "Ada", "weird")), "merge by Ada");
        let old = "Merge git://git.kernel.org/pub/scm/linux/kernel/git/davem/net-2.6";
        assert_eq!(source_of(&c(true, "Linus", old)), "davem/net-2.6");
        let older = "Merge master.kernel.org:/home/rmk/linux-2.6-arm";
        assert_eq!(source_of(&c(true, "Linus", older)), "rmk/linux-2.6-arm");
        let (names, per) = sources(&[c(false, "Linus", "Linux-2.6.12-rc2"), c(false, "Ada", "x")]);
        assert_eq!(names[per[0] as usize], "initial import");
        assert_eq!(names[per[1] as usize], "Ada");
    }

    #[test]
    fn moments_are_spaced_and_sorted() {
        let mut churn = vec![(0u64, 0u64); 100];
        churn[0] = (1_000_000, 0);
        churn[10] = (500, 0);
        churn[11] = (400, 0);
        churn[60] = (0, 300);
        let m = moments(&churn, 3);
        let cs: Vec<usize> = m.iter().map(|m| m.commit).collect();
        assert_eq!(cs, [10, 60]);
        assert!(!cs.contains(&0) && !cs.contains(&11));
    }
}
