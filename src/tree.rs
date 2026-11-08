//! Directory tree over files sorted by path: every node covers a contiguous file id range.

use std::collections::HashMap;

#[derive(Debug)]
pub struct Node {
    pub name: String,
    pub path: String,
    pub parent: Option<usize>,
    /// File id range covered by this node.
    pub lo: u32,
    pub hi: u32,
    /// Child nodes, ordered by peak size (largest first) once `order_by_peak` ran.
    pub children: Vec<usize>,
    pub is_file: bool,
    pub peak: i64,
}

pub struct Tree {
    pub nodes: Vec<Node>,
}

impl Tree {
    /// `files` must be sorted (the builder guarantees it).
    pub fn new(files: &[String]) -> Self {
        let mut nodes = vec![Node {
            name: String::new(),
            path: String::new(),
            parent: None,
            lo: 0,
            hi: files.len() as u32,
            children: Vec::new(),
            is_file: false,
            peak: 0,
        }];
        let mut dirs: HashMap<String, usize> = HashMap::new();
        for (id, f) in files.iter().enumerate() {
            let id = id as u32;
            let mut parent = 0usize;
            let mut prefix = String::new();
            let parts: Vec<&str> = f.split('/').collect();
            for (k, part) in parts.iter().enumerate() {
                if !prefix.is_empty() {
                    prefix.push('/');
                }
                prefix.push_str(part);
                let last = k + 1 == parts.len();
                let idx = if last {
                    None
                } else {
                    dirs.get(&prefix).copied()
                };
                let idx = match idx {
                    Some(i) => i,
                    None => {
                        nodes.push(Node {
                            name: part.to_string(),
                            path: prefix.clone(),
                            parent: Some(parent),
                            lo: id,
                            hi: id + 1,
                            children: Vec::new(),
                            is_file: last,
                            peak: 0,
                        });
                        let i = nodes.len() - 1;
                        nodes[parent].children.push(i);
                        if !last {
                            dirs.insert(prefix.clone(), i);
                        }
                        i
                    }
                };
                nodes[idx].hi = nodes[idx].hi.max(id + 1);
                parent = idx;
            }
        }
        Tree { nodes }
    }

    /// Sort every node's children by the largest size they reached at any checkpoint.
    /// A fixed order keeps each tile in place while scrubbing.
    pub fn order_by_peak<'a>(&mut self, checkpoints: impl Iterator<Item = &'a [i64]>) {
        let mut prefix = Vec::new();
        for counts in checkpoints {
            prefix.clear();
            prefix.push(0i64);
            let mut s = 0;
            for &c in counts {
                s += c;
                prefix.push(s);
            }
            for n in &mut self.nodes {
                n.peak = n.peak.max(prefix[n.hi as usize] - prefix[n.lo as usize]);
            }
        }
        let peaks: Vec<i64> = self.nodes.iter().map(|n| n.peak).collect();
        for n in &mut self.nodes {
            n.children.sort_by_key(|&c| std::cmp::Reverse(peaks[c]));
        }
    }

    pub fn find(&self, path: &str) -> Option<usize> {
        let mut cur = 0;
        for part in path.split('/').filter(|p| !p.is_empty()) {
            cur = *self.nodes[cur]
                .children
                .iter()
                .find(|&&c| self.nodes[c].name == part && !self.nodes[c].is_file)?;
        }
        Some(cur)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ranges_cover_subtrees() {
        let mut files: Vec<String> = ["a/x.c", "a/b/y.c", "a/b/z.c", "a-b.c", "c", "a/b/c/w"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        files.sort();
        let t = Tree::new(&files);
        for n in &t.nodes {
            let want: Vec<u32> = (0..files.len() as u32)
                .filter(|&i| {
                    let f = &files[i as usize];
                    n.path.is_empty()
                        || f == &n.path
                        || (!n.is_file && f.starts_with(&format!("{}/", n.path)))
                })
                .collect();
            let got: Vec<u32> = (n.lo..n.hi).collect();
            assert_eq!(got, want, "node {}", n.path);
        }
        let b = t.find("a/b").unwrap();
        assert_eq!(t.nodes[b].children.len(), 3);
        assert!(t.find("a/zz").is_none());
    }
}
