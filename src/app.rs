//! The window: indexing screen, then the zoomable age map, provenance panel and timeline.

use std::path::{Path, PathBuf};
use std::sync::mpsc::{Receiver, Sender, channel};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use gpui::{
    Bounds, Context, FocusHandle, Hsla, KeyDownEvent, MouseButton, MouseDownEvent, MouseMoveEvent,
    PathBuilder, Pixels, Render, Rgba, SharedString, Window, canvas, div, point, prelude::*, px,
    quad, size,
};
use memmap2::Mmap;

use crate::build::{self, Meta, Progress};
use crate::engine::{Done, Engine};
use crate::story::{self, Moment};
use crate::tree::Tree;
use crate::treemap::{Rect, layout};

pub(crate) const HEADER_H: f32 = 64.0;
pub(crate) const TIMELINE_H: f32 = 84.0;
pub(crate) const PANEL_W: f32 = 300.0;
pub(crate) const PAD: f32 = 12.0;
const PLAY_SECONDS: f64 = 40.0;
pub(crate) const MOMENTS: usize = 14;

pub struct Data {
    pub meta: Meta,
    pub engine: Engine<Mmap>,
    pub tree: Tree,
    pub src_names: Vec<String>,
    pub src_of: Vec<u32>,
    pub moments: Vec<Moment>,
}

/// Load the cache for `repo`, building it first if missing or out of date.
pub fn load_or_build(
    repo: &Path,
    rebuild: bool,
    progress: &Progress,
    phase: &Mutex<String>,
) -> Result<Data, String> {
    let dir = build::cache_dir(repo);
    let head = build::head(repo)?;
    let fresh = !rebuild
        && std::fs::read(dir.join("meta.json"))
            .ok()
            .and_then(|b| serde_json::from_slice::<MetaHead>(&b).ok())
            .is_some_and(|m| m.format == build::FORMAT && m.head == head);
    if !fresh {
        *phase.lock().expect("phase") = "Indexing history".into();
        let t0 = Instant::now();
        build::build(repo, &dir, progress)?;
        eprintln!(
            "index built in {:.1}s: {}",
            t0.elapsed().as_secs_f64(),
            dir.display()
        );
    }
    *phase.lock().expect("phase") = "Loading".into();
    load(&dir)
}

#[derive(serde::Deserialize)]
struct MetaHead {
    format: u32,
    head: String,
}

pub fn load(dir: &Path) -> Result<Data, String> {
    let t0 = Instant::now();
    let meta: Meta =
        serde_json::from_slice(&std::fs::read(dir.join("meta.json")).map_err(|e| e.to_string())?)
            .map_err(|e| e.to_string())?;
    let times = meta.commits.iter().map(|c| c.time).collect();
    let engine = Engine::open(dir, times, meta.files.len()).map_err(|e| e.to_string())?;
    let mut tree = Tree::new(&meta.files);
    tree.order_by_peak(engine.checkpoint_counts());
    let (src_names, src_of) = story::sources(&meta.commits);
    let moments = story::moments(&engine.churn, MOMENTS);
    let mut engine = engine;
    engine.set_sources(src_of.clone());
    eprintln!(
        "loaded {} commits, {} files, {} sources in {:.2}s",
        engine.len(),
        meta.files.len(),
        src_names.len(),
        t0.elapsed().as_secs_f64()
    );
    Ok(Data {
        meta,
        engine,
        tree,
        src_names,
        src_of,
        moments,
    })
}

type Pending = Arc<Mutex<Option<Result<Data, String>>>>;

enum Screen {
    Loading {
        progress: Arc<Progress>,
        phase: Arc<Mutex<String>>,
        result: Pending,
    },
    Ready(Box<Ready>),
    Failed(String),
}

pub struct Tile {
    pub rect: Rect,
    pub node: usize,
    /// A directory frame drawn behind its children (title bar on top).
    pub frame: bool,
    pub color: Hsla,
    pub label: Option<SharedString>,
}

#[derive(Default)]
struct Stats {
    frame_intervals: Vec<Duration>,
    render_times: Vec<Duration>,
    seek_times: Vec<Duration>,
    rescans: Vec<Duration>,
    last_frame: Option<Instant>,
    last_report: Option<Instant>,
}

struct Ready {
    data: Data,
    view: usize,
    pos: f64,
    playing: bool,
    dragging: bool,
    mouse: Option<(f32, f32)>,
    tiles: Vec<Tile>,
    /// Provenance rows of the current view: (source, label, lines). `None` = "autres".
    prov: Vec<(Option<u32>, String, i64)>,
    prov_stale: bool,
    /// Selected source: the map shows its territory instead of line age.
    focus: Option<u32>,
    /// Commits of the selected source, for the timeline strip.
    focus_ticks: Vec<usize>,
    /// Cohort bands of the timeline, rebuilt only when its rect changes.
    timeline_paths: Option<(Rect, Vec<Band>)>,
    /// Background rescans report here.
    tx: Sender<(Done, Duration)>,
    rx: Receiver<(Done, Duration)>,
    stats: Stats,
    fps: f64,
}

pub struct Opts {
    pub bench: bool,
    pub at: Option<f64>,
    pub start_path: Option<String>,
    /// Open on this source's territory (exact name from the provenance panel).
    pub source: Option<String>,
}

pub struct Scrubber {
    screen: Screen,
    opts: Opts,
    focus: FocusHandle,
}

impl Scrubber {
    pub fn new(repo: PathBuf, rebuild: bool, opts: Opts, cx: &mut Context<Self>) -> Self {
        let progress = Arc::new(Progress::default());
        let phase = Arc::new(Mutex::new("Preparing".to_string()));
        let result: Pending = Arc::new(Mutex::new(None));
        {
            let (progress, phase, result) = (progress.clone(), phase.clone(), result.clone());
            std::thread::spawn(move || {
                let r = load_or_build(&repo, rebuild, &progress, &phase);
                *result.lock().expect("result") = Some(r);
            });
        }
        Scrubber {
            screen: Screen::Loading {
                progress,
                phase,
                result,
            },
            opts,
            focus: cx.focus_handle(),
        }
    }

    pub fn focus_handle(&self) -> &FocusHandle {
        &self.focus
    }

    fn poll_loading(&mut self) {
        let Screen::Loading { result, .. } = &self.screen else {
            return;
        };
        let Some(r) = result.lock().expect("result").take() else {
            return;
        };
        self.screen = match r {
            Ok(mut data) => {
                let n = data.engine.len();
                let start = self
                    .opts
                    .at
                    .map_or(n - 1, |f| (f.clamp(0.0, 1.0) * (n - 1) as f64) as usize);
                let start = if self.opts.bench { 0 } else { start };
                data.engine.seek(start);
                let view = self
                    .opts
                    .start_path
                    .as_deref()
                    .and_then(|p| data.tree.find(p))
                    .unwrap_or(0);
                let (tx, rx) = channel();
                Screen::Ready(Box::new(Ready {
                    data,
                    view,
                    pos: start as f64,
                    playing: self.opts.bench,
                    dragging: false,
                    mouse: None,
                    tiles: Vec::new(),
                    prov: Vec::new(),
                    prov_stale: true,
                    focus: None,
                    focus_ticks: Vec::new(),
                    timeline_paths: None,
                    tx,
                    rx,
                    stats: Stats::default(),
                    fps: 0.0,
                }))
            }
            Err(e) => {
                eprintln!("error: {e}");
                Screen::Failed(e)
            }
        };
        if let (Screen::Ready(r), Some(name)) = (&mut self.screen, self.opts.source.as_deref()) {
            match r.data.src_names.iter().position(|s| s == name) {
                Some(i) => r.set_focus(Some(i as u32)),
                None => eprintln!("unknown source: {name}"),
            }
        }
    }
}

/// Map area for a window of `vw` x `vh` logical pixels.
pub(crate) fn map_rect(vw: f32, vh: f32) -> Rect {
    Rect {
        x: PAD,
        y: HEADER_H,
        w: vw - 3.0 * PAD - PANEL_W,
        h: vh - HEADER_H - TIMELINE_H,
    }
}

/// Timeline bar area for a window of `vw` x `vh` logical pixels.
pub(crate) fn timeline_rect(vw: f32, vh: f32) -> Rect {
    Rect {
        x: PAD,
        y: vh - TIMELINE_H + 18.0,
        w: vw - 2.0 * PAD,
        h: TIMELINE_H - 34.0,
    }
}

pub(crate) fn rgba(hex: u32) -> Hsla {
    gpui::rgb(hex).into()
}

/// Sequential ramp: fresh code bright yellow, old code deep violet.
const AGE_STOPS: [(f64, u32); 6] = [
    (0.0, 0xfcf3a4),
    (2.0, 0xf6a623),
    (5.0, 0xe0503b),
    (9.0, 0x9a2a6b),
    (14.0, 0x4b2a86),
    (20.0, 0x2a2f6e),
];

pub(crate) fn age_color(years: f64) -> Hsla {
    let y = years.clamp(0.0, 20.0);
    let i = AGE_STOPS
        .iter()
        .rposition(|s| s.0 <= y)
        .unwrap_or(0)
        .min(AGE_STOPS.len() - 2);
    let (a, b) = (AGE_STOPS[i], AGE_STOPS[i + 1]);
    let t = ((y - a.0) / (b.0 - a.0)) as f32;
    let c = |h: u32, s: u32| ((h >> s) & 0xff) as f32 / 255.0;
    let mix = |s| c(a.1, s) + (c(b.1, s) - c(a.1, s)) * t;
    Rgba {
        r: mix(16),
        g: mix(8),
        b: mix(0),
        a: 1.0,
    }
    .into()
}

/// Accent for the selected source in the panel and its commits on the timeline.
pub(crate) const TERRITORY: u32 = 0x38bdf8;
/// What a tile fades to when none of its lines come from the selected source.
const FADED: u32 = 0x1d1d22;

/// The selected source's lines keep their age colour; the rest of the tile fades out.
/// Square root of the share so that a 5 % presence is still visible.
fn territory_color(age_years: f64, share: f64) -> Hsla {
    let t = share.clamp(0.0, 1.0).sqrt() as f32;
    let (a, b) = (
        Hsla::from(gpui::rgb(FADED)).to_rgb(),
        age_color(age_years).to_rgb(),
    );
    Rgba {
        r: a.r + (b.r - a.r) * t,
        g: a.g + (b.g - a.g) * t,
        b: a.b + (b.b - a.b) * t,
        a: 1.0,
    }
    .into()
}

/// Age colour, or the selected source's territory (grey while it computes).
fn tile_color(
    e: &Engine<Mmap>,
    focus: Option<u32>,
    lo: usize,
    hi: usize,
    a: crate::engine::Agg,
) -> Hsla {
    match focus {
        None => age_color(e.mean_age_years(a).unwrap_or(0.0)),
        Some(_) => match e.territory(lo, hi) {
            Some(mine) => territory_color(
                e.mean_age_years(mine).unwrap_or(0.0),
                mine.count as f64 / a.count.max(1) as f64,
            ),
            None => rgba(0x3f3f46),
        },
    }
}

pub fn date(unix: i64) -> String {
    // Civil-from-days (Howard Hinnant).
    let z = unix.div_euclid(86400) + 719468;
    let era = z.div_euclid(146097);
    let doe = z - era * 146097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    format!("{y:04}-{m:02}-{d:02}")
}

pub(crate) fn thousands(n: i64) -> String {
    let s = n.abs().to_string();
    let mut out = String::from(if n < 0 { "-" } else { "" });
    for (i, ch) in s.chars().enumerate() {
        if i > 0 && (s.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(ch);
    }
    out
}

fn bounds(r: Rect) -> Bounds<Pixels> {
    Bounds {
        origin: point(px(r.x), px(r.y)),
        size: size(px(r.w), px(r.h)),
    }
}

fn p95(v: &mut [Duration]) -> f64 {
    if v.is_empty() {
        return 0.0;
    }
    v.sort();
    v[(v.len() * 95 / 100).min(v.len() - 1)].as_secs_f64() * 1e3
}

fn contains(r: Rect, x: f32, y: f32) -> bool {
    x >= r.x && x < r.x + r.w && y >= r.y && y < r.y + r.h
}

/// Panel rows for per-source line counts: top 10, the selected source if it is not among
/// them, then "N other sources". `None` marks the aggregate row.
pub(crate) fn provenance_rows(
    data: &Data,
    focus: Option<u32>,
    by_src: &[i64],
) -> Vec<(Option<u32>, String, i64)> {
    let mut rows: Vec<(usize, i64)> = by_src
        .iter()
        .copied()
        .enumerate()
        .filter(|r| r.1 > 0)
        .collect();
    rows.sort_by_key(|r| std::cmp::Reverse(r.1));
    let rest: i64 = rows.iter().skip(10).map(|r| r.1).sum();
    let mut out: Vec<(Option<u32>, String, i64)> = rows
        .iter()
        .take(10)
        .map(|&(s, k)| (Some(s as u32), data.src_names[s].clone(), k))
        .collect();
    // Keep the selected source visible even when it is not in the top 10.
    if let Some(f) = focus
        && !out.iter().any(|r| r.0 == Some(f))
        && let Some(&(_, k)) = rows.iter().find(|r| r.0 as u32 == f)
    {
        out.push((Some(f), data.src_names[f as usize].clone(), k));
    }
    if rest > 0 {
        out.push((
            None,
            format!("{} other sources", thousands(rows.len() as i64 - 10)),
            rest,
        ));
    }
    out
}

/// Two-level treemap of `view`: its children, and inside each directory its own children.
/// Shared by the window and the video recorder so both show exactly the same map.
pub fn layout_tiles(data: &Data, view: usize, focus: Option<u32>, map: Rect) -> Vec<Tile> {
    let mut tiles = Vec::new();
    let (tree, e) = (&data.tree, &data.engine);
    let kids = &tree.nodes[view].children;
    let w: Vec<f64> = kids
        .iter()
        .map(|&k| {
            e.agg(tree.nodes[k].lo as usize, tree.nodes[k].hi as usize)
                .count
                .max(0) as f64
        })
        .collect();
    for (i, r) in layout(&w, map).into_iter().enumerate() {
        if r.w < 1.0 || r.h < 1.0 {
            continue;
        }
        let k = kids[i];
        let node = &tree.nodes[k];
        if node.is_file || node.children.is_empty() {
            let a = e.agg(node.lo as usize, node.hi as usize);
            tiles.push(Tile {
                rect: inset(r, 0.5),
                node: k,
                frame: false,
                color: tile_color(e, focus, node.lo as usize, node.hi as usize, a),
                label: (r.w > 56.0 && r.h > 18.0).then(|| node.name.clone().into()),
            });
            continue;
        }
        let titled = r.w > 70.0 && r.h > 40.0;
        tiles.push(Tile {
            rect: r,
            node: k,
            frame: true,
            color: rgba(0x26262b),
            label: titled.then(|| format!("{}/", node.name).into()),
        });
        let top = if titled { 16.0 } else { 1.0 };
        let inner = Rect {
            x: r.x + 1.0,
            y: r.y + top,
            w: (r.w - 2.0).max(0.0),
            h: (r.h - top - 1.0).max(0.0),
        };
        let gk = &node.children;
        let gw: Vec<f64> = gk
            .iter()
            .map(|&g| {
                e.agg(tree.nodes[g].lo as usize, tree.nodes[g].hi as usize)
                    .count
                    .max(0) as f64
            })
            .collect();
        for (j, q) in layout(&gw, inner).into_iter().enumerate() {
            if q.w < 0.5 || q.h < 0.5 {
                continue;
            }
            let g = &tree.nodes[gk[j]];
            let a = e.agg(g.lo as usize, g.hi as usize);
            let name = if g.is_file {
                g.name.clone()
            } else {
                format!("{}/", g.name)
            };
            tiles.push(Tile {
                rect: inset(q, 0.5),
                node: gk[j],
                frame: false,
                color: tile_color(e, focus, g.lo as usize, g.hi as usize, a),
                label: (q.w > 56.0 && q.h > 18.0).then(|| name.into()),
            });
        }
    }
    tiles
}

impl Ready {
    fn n(&self) -> usize {
        self.data.engine.len()
    }

    fn seek(&mut self, c: usize) {
        let t0 = Instant::now();
        self.data.engine.seek(c);
        self.stats.seek_times.push(t0.elapsed());
    }

    fn commit_x(&self, c: usize, tl: Rect) -> f32 {
        tl.x + tl.w * c as f32 / (self.n() - 1).max(1) as f32
    }

    fn seek_to_x(&mut self, x: f32, tl: Rect) {
        let f = ((x - tl.x) / tl.w).clamp(0.0, 1.0) as f64;
        self.pos = f * (self.n() - 1) as f64;
        self.seek(self.pos.round() as usize);
    }

    fn moment_at(&self, x: f32, y: f32, tl: Rect) -> Option<usize> {
        if y < tl.y - 16.0 || y > tl.y + tl.h {
            return None;
        }
        self.data
            .moments
            .iter()
            .enumerate()
            .filter(|(_, m)| (self.commit_x(m.commit, tl) - x).abs() <= 5.0)
            .min_by(|a, b| {
                let da = (self.commit_x(a.1.commit, tl) - x).abs();
                let db = (self.commit_x(b.1.commit, tl) - x).abs();
                da.total_cmp(&db)
            })
            .map(|(i, _)| i)
    }

    fn build_tiles(&mut self, map: Rect) {
        self.tiles = layout_tiles(&self.data, self.view, self.focus, map);
    }

    fn hovered_tile(&self) -> Option<&Tile> {
        let (mx, my) = self.mouse?;
        self.tiles.iter().rev().find(|t| contains(t.rect, mx, my))
    }

    /// Zoom target for a click: the deepest directory under the cursor below the view.
    fn zoom_target(&self, x: f32, y: f32) -> Option<usize> {
        let nodes = &self.data.tree.nodes;
        let hit: Vec<&Tile> = self
            .tiles
            .iter()
            .filter(|t| contains(t.rect, x, y))
            .collect();
        hit.iter()
            .rev()
            .map(|t| t.node)
            .find(|&n| !nodes[n].is_file && !nodes[n].children.is_empty())
    }

    /// Keep both trackers pointed at what is on screen, run their rescans on background
    /// threads, and rebuild the provenance rows. Playback steps keep the trackers current
    /// for free; only a new view, a new source or a long jump costs a rescan, and that
    /// never blocks a frame. Returns true while a rescan is in flight.
    fn refresh_trackers(&mut self) -> bool {
        let e = &mut self.data.engine;
        while let Ok((done, took)) = self.rx.try_recv() {
            e.accept(done);
            self.stats.rescans.push(took);
        }
        let node = &self.data.tree.nodes[self.view];
        e.watch_provenance(node.lo, node.hi);
        e.watch_territory(self.focus);
        for job in e.take_jobs() {
            let tx = self.tx.clone();
            std::thread::spawn(move || {
                let t0 = Instant::now();
                let done = job.run();
                let _ = tx.send((done, t0.elapsed()));
            });
        }
        let busy = e.busy();
        let Some(by_src) = self.data.engine.provenance() else {
            self.prov_stale = true;
            return true;
        };
        self.prov = provenance_rows(&self.data, self.focus, by_src);
        self.prov_stale = false;
        busy
    }

    fn set_focus(&mut self, src: Option<u32>) {
        self.focus = src;
        self.focus_ticks = match src {
            Some(f) => (0..self.n())
                .filter(|&c| self.data.src_of[c] == f && self.data.engine.churn[c].0 > 0)
                .collect(),
            None => Vec::new(),
        };
    }

    fn record_frame(&mut self, render_time: Duration, bench: bool) -> bool {
        let now = Instant::now();
        // Idle frames only happen on input events: their rate measures the mouse, not us.
        if !self.playing && !self.dragging && !bench {
            self.stats.last_frame = None;
            self.fps = 0.0;
            return false;
        }
        if let Some(prev) = self.stats.last_frame {
            self.stats.frame_intervals.push(now - prev);
        }
        self.stats.last_frame = Some(now);
        self.stats.render_times.push(render_time);
        let last = *self.stats.last_report.get_or_insert(now);
        let done = bench && !self.playing;
        if now - last >= Duration::from_secs(1) || done {
            let s = &mut self.stats;
            let k = s.frame_intervals.len().max(1);
            self.fps = k as f64
                / s.frame_intervals
                    .iter()
                    .sum::<Duration>()
                    .as_secs_f64()
                    .max(1e-9);
            eprintln!(
                "fps={:.1} frame_p95={:.1}ms render_p95={:.2}ms seek_p95={:.2}ms seek_max={:.2}ms rescans={} rescan_max={:.1}ms commit={}",
                self.fps,
                p95(&mut s.frame_intervals),
                p95(&mut s.render_times),
                p95(&mut s.seek_times),
                s.seek_times
                    .iter()
                    .max()
                    .copied()
                    .unwrap_or_default()
                    .as_secs_f64()
                    * 1e3,
                s.rescans.len(),
                s.rescans
                    .iter()
                    .max()
                    .copied()
                    .unwrap_or_default()
                    .as_secs_f64()
                    * 1e3,
                self.data.engine.current(),
            );
            s.rescans.clear();
            s.frame_intervals.clear();
            s.render_times.clear();
            s.seek_times.clear();
            s.last_report = Some(now);
        }
        done
    }
}

/// A filled timeline band and its colour.
type Band = (gpui::Path<Pixels>, Hsla);

/// Polygons (outline points) of each birth-year cohort band across the timeline, with
/// the cohort's colour (its age at HEAD, same ramp as the map). Shared with the recorder.
pub(crate) fn cohort_polygons(e: &Engine<Mmap>, tl: Rect) -> Vec<(Vec<(f32, f32)>, Hsla)> {
    let n = e.len();
    let peak = e.totals.iter().copied().max().unwrap_or(1).max(1) as f32;
    let cols = tl.w.max(2.0) as usize;
    let ny = e.cohort_years.len();
    // edges[i][k]: y of the bottom of cohort k at column i (cumulative stack).
    let mut edges = vec![vec![0f32; ny + 1]; cols];
    for (i, col) in edges.iter_mut().enumerate() {
        let cc = (((i + 1) * n) / cols).clamp(1, n) - 1;
        let mut y = tl.y + tl.h;
        col[0] = y;
        for (k, &v) in e.cohort_row(cc).iter().enumerate() {
            y -= tl.h * v.max(0) as f32 / peak;
            col[k + 1] = y;
        }
    }
    let last_year = *e.cohort_years.last().unwrap_or(&1970);
    let x = |i: usize| tl.x + tl.w * i as f32 / (cols - 1) as f32;
    let mut out = Vec::new();
    for k in 0..ny {
        if edges.iter().all(|c| c[k] - c[k + 1] < 0.05) {
            continue; // empty band
        }
        let mut pts: Vec<(f32, f32)> = (0..cols).map(|i| (x(i), edges[i][k + 1])).collect();
        pts.extend((0..cols).rev().map(|i| (x(i), edges[i][k])));
        out.push((pts, age_color((last_year - e.cohort_years[k]) as f64)));
    }
    out
}

/// The cohort bands as GPUI paths (cached by the window).
fn cohort_paths(e: &Engine<Mmap>, tl: Rect) -> Vec<Band> {
    cohort_polygons(e, tl)
        .into_iter()
        .filter_map(|(pts, col)| {
            let mut b = PathBuilder::fill();
            b.move_to(point(px(pts[0].0), px(pts[0].1)));
            for &(x, y) in &pts[1..] {
                b.line_to(point(px(x), px(y)));
            }
            b.close();
            b.build().ok().map(|p| (p, col))
        })
        .collect()
}

fn inset(r: Rect, d: f32) -> Rect {
    Rect {
        x: r.x + d,
        y: r.y + d,
        w: (r.w - 2.0 * d).max(0.5),
        h: (r.h - 2.0 * d).max(0.5),
    }
}

fn loading_view(progress: &Progress, phase: &str) -> impl IntoElement {
    let total = progress.total.load(std::sync::atomic::Ordering::Relaxed);
    let done = progress.done.load(std::sync::atomic::Ordering::Relaxed);
    let f = if total > 0 {
        done as f32 / total as f32
    } else {
        0.0
    };
    div()
        .size_full()
        .flex()
        .flex_col()
        .items_center()
        .justify_center()
        .gap_3()
        .bg(rgba(0x111114))
        .text_color(rgba(0xe4e4e7))
        .child(div().text_size(px(18.0)).child(phase.to_string()))
        .child(
            div()
                .w(px(420.0))
                .h(px(6.0))
                .bg(rgba(0x26262b))
                .child(div().h_full().w(px(420.0 * f)).bg(rgba(0xfcf3a4))),
        )
        .child(
            div()
                .text_size(px(12.0))
                .text_color(rgba(0xa1a1aa))
                .child(if total > 0 {
                    format!(
                        "{} / {} commits",
                        thousands(done as i64),
                        thousands(total as i64)
                    )
                } else {
                    "reading history".to_string()
                }),
        )
}

impl Render for Scrubber {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.poll_loading();
        let bench = self.opts.bench;
        let focus = self.focus.clone();
        let r = match &mut self.screen {
            Screen::Loading {
                progress, phase, ..
            } => {
                window.request_animation_frame();
                let phase = phase.lock().expect("phase").clone();
                return div()
                    .size_full()
                    .child(loading_view(progress, &phase))
                    .into_any_element();
            }
            Screen::Failed(e) => {
                return div()
                    .size_full()
                    .flex()
                    .items_center()
                    .justify_center()
                    .bg(rgba(0x111114))
                    .text_color(rgba(0xf87171))
                    .child(format!("Could not index this repository: {e}"))
                    .into_any_element();
            }
            Screen::Ready(r) => r,
        };

        let t0 = Instant::now();
        let vp = window.viewport_size();
        let (vw, vh) = (f32::from(vp.width), f32::from(vp.height));
        let n = r.n();
        if r.playing {
            r.pos += n as f64 / (PLAY_SECONDS * 60.0);
            if r.pos >= (n - 1) as f64 {
                r.pos = (n - 1) as f64;
                r.playing = false;
            }
            r.seek(r.pos.round() as usize);
            window.request_animation_frame();
        }
        let map = map_rect(vw, vh);
        r.build_tiles(map);
        if r.refresh_trackers() {
            window.request_animation_frame(); // poll the background rescan
        }

        let c = r.data.engine.current();
        let tl = timeline_rect(vw, vh);
        let view_node = &r.data.tree.nodes[r.view];
        let view_agg = r
            .data
            .engine
            .agg(view_node.lo as usize, view_node.hi as usize);

        // Hover text: a moment marker wins over a tile.
        let hover_moment = r.mouse.and_then(|(x, y)| r.moment_at(x, y, tl));
        let info: SharedString = if let Some(i) = hover_moment {
            let m = &r.data.moments[i];
            let cm = &r.data.meta.commits[m.commit];
            format!(
                "{}  ·  {}  ·  +{} / -{} lines  ·  {}",
                date(cm.time),
                r.data.src_names[r.data.src_of[m.commit] as usize],
                thousands(m.born as i64),
                thousands(m.dead as i64),
                cm.subject.chars().take(70).collect::<String>()
            )
        } else if let Some((x, _)) = r.mouse.filter(|&(x, y)| contains(tl, x, y)) {
            let hc = (((x - tl.x) / tl.w).clamp(0.0, 1.0) * (n - 1) as f32) as usize;
            let row = r.data.engine.cohort_row(hc);
            let total = r.data.engine.totals[hc].max(1);
            let first = r.data.engine.cohort_years.first().copied().unwrap_or(1970);
            format!(
                "{}  ·  {} live lines  ·  {:.1}% still from {}",
                date(r.data.meta.commits[hc].time),
                thousands(r.data.engine.totals[hc]),
                100.0 * row.first().copied().unwrap_or(0) as f64 / total as f64,
                first
            )
        } else if let Some(t) = r.hovered_tile() {
            let nd = &r.data.tree.nodes[t.node];
            let a = r.data.engine.agg(nd.lo as usize, nd.hi as usize);
            match (
                r.focus,
                r.data.engine.territory(nd.lo as usize, nd.hi as usize),
            ) {
                (Some(f), Some(mine)) => format!(
                    "{}  ·  {} lines  ·  {} from {} ({:.1}%, mean age {:.1} y)",
                    nd.path,
                    thousands(a.count),
                    thousands(mine.count),
                    r.data.src_names[f as usize],
                    100.0 * mine.count as f64 / a.count.max(1) as f64,
                    r.data.engine.mean_age_years(mine).unwrap_or(0.0)
                ),
                _ => format!(
                    "{}  ·  {} lines  ·  mean age {:.1} y",
                    nd.path,
                    thousands(a.count),
                    r.data.engine.mean_age_years(a).unwrap_or(0.0)
                ),
            }
        } else {
            String::new()
        }
        .into();

        let quads: Vec<(Bounds<Pixels>, Hsla)> =
            r.tiles.iter().map(|t| (bounds(t.rect), t.color)).collect();
        let hover_rect = r
            .hovered_tile()
            .filter(|t| !t.frame)
            .map(|t| bounds(t.rect));
        // Timeline: live lines stacked by the year they were born (oldest at the bottom).
        // One filled path per cohort, cached until the timeline is resized: 15k quads per
        // frame cost ~40 fps, 20-odd cached paths cost nothing. The part after the
        // playhead is dimmed with a single overlay.
        let head_x = r.commit_x(c, tl);
        if r.timeline_paths.as_ref().is_none_or(|(k, _)| *k != tl) {
            r.timeline_paths = Some((tl, cohort_paths(&r.data.engine, tl)));
        }
        let stack = r
            .timeline_paths
            .as_ref()
            .map(|(_, p)| p.clone())
            .unwrap_or_default();
        let marks: Vec<(f32, bool)> = r
            .data
            .moments
            .iter()
            .enumerate()
            .map(|(i, m)| (r.commit_x(m.commit, tl), Some(i) == hover_moment))
            .collect();

        let mut ticks: Vec<f32> = r
            .focus_ticks
            .iter()
            .map(|&c| r.commit_x(c, tl).floor())
            .collect();
        ticks.dedup();
        let sel = r.focus;
        let labels = r.tiles.iter().filter_map(|t| {
            let text = t.label.clone()?;
            Some(
                div()
                    .absolute()
                    .left(px(t.rect.x + 4.0))
                    .top(px(t.rect.y + 2.0))
                    .w(px((t.rect.w - 8.0).max(0.0)))
                    .overflow_hidden()
                    .whitespace_nowrap()
                    .text_size(px(if t.frame { 11.0 } else { 10.0 }))
                    .text_color(if t.frame {
                        rgba(0xd4d4d8)
                    } else {
                        rgba(0x0b0b0e).opacity(0.75)
                    })
                    .child(text),
            )
        });
        let first_year = date(r.data.meta.commits[0].time)[..4]
            .parse::<i64>()
            .unwrap_or(2005);
        let last_year = date(r.data.meta.commits[n - 1].time)[..4]
            .parse::<i64>()
            .unwrap_or(first_year);
        let step = ((last_year - first_year) / 8).max(1);
        let years: Vec<_> = (first_year + 1..=last_year)
            .filter(|y| (y - first_year) % step == 0)
            .filter_map(|y| {
                let ts = (y - 1970) as f64 * 365.2425 * 86400.0;
                let i = r
                    .data
                    .meta
                    .commits
                    .partition_point(|cm| (cm.time as f64) < ts);
                (i > 0 && i < n).then(|| {
                    div()
                        .absolute()
                        .left(px(r.commit_x(i, tl) - 12.0))
                        .top(px(tl.y + tl.h + 2.0))
                        .text_size(px(10.0))
                        .text_color(rgba(0x71717a))
                        .child(y.to_string())
                })
            })
            .collect();

        // Breadcrumb: each ancestor is clickable.
        let mut chain = vec![r.view];
        while let Some(p) = r.data.tree.nodes[*chain.last().expect("chain")].parent {
            chain.push(p);
        }
        chain.reverse();
        let repo_name: SharedString = Path::new(&r.data.meta.repo)
            .file_name()
            .map_or("repo".into(), |s| {
                s.to_string_lossy().trim_end_matches(".git").to_string()
            })
            .into();
        let crumbs = chain.into_iter().enumerate().map(|(i, nidx)| {
            let name: SharedString = if i == 0 {
                repo_name.clone()
            } else {
                r.data.tree.nodes[nidx].name.clone().into()
            };
            div()
                .id(("crumb", nidx))
                .flex()
                .child(if i == 0 {
                    String::new()
                } else {
                    " / ".to_string()
                })
                .child(
                    div()
                        .text_color(rgba(0xe4e4e7))
                        .hover(|s| s.text_color(rgba(0xfcf3a4)))
                        .child(name),
                )
                .on_click(cx.listener(move |this, _, _, cx| {
                    if let Screen::Ready(r) = &mut this.screen {
                        r.view = nidx;
                        cx.notify();
                    }
                }))
        });

        let total = view_agg.count;
        let panel_x = vw - PAD - PANEL_W;
        let prov_total: i64 = r.prov.iter().map(|p| p.2).sum::<i64>().max(1);
        let prov_rows = r.prov.iter().map(|(src, name, k)| {
            let f = *k as f32 / prov_total as f32;
            let selected = src.is_some() && *src == sel;
            let bar = if selected { TERRITORY } else { 0xa1a1aa };
            let row = div()
                .id(("src", src.map_or(u32::MAX as usize, |s| s as usize)))
                .flex()
                .flex_col()
                .gap(px(2.0))
                .px(px(4.0))
                .py(px(2.0))
                .when(selected, |d| d.bg(rgba(0x26262b)))
                .when(src.is_some(), |d| d.hover(|h| h.bg(rgba(0x222227))))
                .child(
                    div()
                        .flex()
                        .justify_between()
                        .text_size(px(11.0))
                        .child(
                            div()
                                .overflow_hidden()
                                .whitespace_nowrap()
                                .text_color(if selected {
                                    rgba(TERRITORY)
                                } else {
                                    rgba(0xd4d4d8)
                                })
                                .child(name.clone()),
                        )
                        .child(
                            div()
                                .text_color(rgba(0xa1a1aa))
                                .child(format!("{:.1}%", f * 100.0)),
                        ),
                )
                .child(div().h(px(3.0)).w(px((PANEL_W - 32.0) * f)).bg(rgba(bar)));
            match *src {
                Some(s) => row
                    .on_click(cx.listener(move |this, _, _, cx| {
                        if let Screen::Ready(r) = &mut this.screen {
                            r.set_focus(if r.focus == Some(s) { None } else { Some(s) });
                            cx.notify();
                        }
                    }))
                    .into_any_element(),
                None => row.into_any_element(),
            }
        });
        let stale = r.prov_stale;
        let legend_ticks = [0.0, 5.0, 10.0, 15.0, 20.0];

        let el = div()
            .id("root")
            .track_focus(&focus)
            .size_full()
            .relative()
            .bg(rgba(0x111114))
            .font_family(".SystemUIFont")
            .on_key_down(cx.listener(|this, ev: &KeyDownEvent, _, cx| {
                let Screen::Ready(r) = &mut this.screen else { return };
                let step = if ev.keystroke.modifiers.shift { 500 } else { 1 };
                let n = r.n();
                match ev.keystroke.key.as_str() {
                    "space" => {
                        if !r.playing && r.data.engine.current() >= n - 1 {
                            r.pos = 0.0;
                        }
                        r.playing = !r.playing;
                    }
                    "left" => {
                        r.playing = false;
                        r.pos = r.data.engine.current().saturating_sub(step) as f64;
                        r.seek(r.pos as usize);
                    }
                    "right" => {
                        r.playing = false;
                        r.pos = (r.data.engine.current() + step).min(n - 1) as f64;
                        r.seek(r.pos as usize);
                    }
                    "escape" | "backspace" => {
                        if r.focus.is_some() {
                            r.set_focus(None);
                        } else if let Some(p) = r.data.tree.nodes[r.view].parent {
                            r.view = p;
                        }
                    }
                    _ => return,
                }
                cx.notify();
            }))
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, ev: &MouseDownEvent, window, cx| {
                    let Screen::Ready(r) = &mut this.screen else { return };
                    let vp = window.viewport_size();
                    let (vw, vh) = (f32::from(vp.width), f32::from(vp.height));
                    let (x, y) = (f32::from(ev.position.x), f32::from(ev.position.y));
                    let tl = timeline_rect(vw, vh);
                    if let Some(i) = r.moment_at(x, y, tl) {
                        r.playing = false;
                        r.pos = r.data.moments[i].commit as f64;
                        r.seek(r.pos as usize);
                    } else if y >= tl.y - 6.0 && y <= tl.y + tl.h + 6.0 {
                        r.dragging = true;
                        r.playing = false;
                        r.seek_to_x(x, tl);
                    } else if contains(map_rect(vw, vh), x, y)
                        && let Some(z) = r.zoom_target(x, y) {
                            r.view = z;
                        }
                    cx.notify();
                }),
            )
            .on_mouse_move(cx.listener(|this, ev: &MouseMoveEvent, window, cx| {
                let Screen::Ready(r) = &mut this.screen else { return };
                let (x, y) = (f32::from(ev.position.x), f32::from(ev.position.y));
                r.mouse = Some((x, y));
                if r.dragging {
                    let vp = window.viewport_size();
                    r.seek_to_x(x, timeline_rect(f32::from(vp.width), f32::from(vp.height)));
                }
                cx.notify();
            }))
            .on_mouse_up(
                MouseButton::Left,
                cx.listener(|this, _, _, cx| {
                    if let Screen::Ready(r) = &mut this.screen {
                        r.dragging = false;
                        cx.notify();
                    }
                }),
            )
            .child(
                canvas(
                    |_, _, _| {},
                    move |_, _, window, _| {
                        for (b, col) in quads {
                            window.paint_quad(quad(b, px(0.), col, px(0.), gpui::transparent_black(), Default::default()));
                        }
                        if let Some(b) = hover_rect {
                            window.paint_quad(quad(b, px(0.), gpui::transparent_black(), px(1.5), rgba(0xffffff), Default::default()));
                        }
                        window.paint_quad(gpui::fill(bounds(tl), rgba(0x1c1c21)));
                        for (path, col) in stack {
                            window.paint_path(path, col);
                        }
                        let rest = Rect { x: head_x, y: tl.y, w: (tl.x + tl.w - head_x).max(0.0), h: tl.h };
                        window.paint_quad(gpui::fill(bounds(rest), rgba(0x1c1c21).opacity(0.7)));
                        for (x, hot) in marks {
                            let col = if hot { rgba(0xfcf3a4) } else { rgba(0xa1a1aa) };
                            window.paint_quad(gpui::fill(bounds(Rect { x: x - 3.0, y: tl.y - 12.0, w: 6.0, h: 6.0 }), col));
                            window.paint_quad(gpui::fill(bounds(Rect { x: x - 0.5, y: tl.y - 6.0, w: 1.0, h: 6.0 }), col.opacity(0.6)));
                        }
                        for x in ticks {
                            window.paint_quad(gpui::fill(bounds(Rect { x, y: tl.y, w: 1.0, h: tl.h }), rgba(TERRITORY).opacity(0.35)));
                        }
                        window.paint_quad(gpui::fill(bounds(Rect { x: head_x - 1.0, y: tl.y - 4.0, w: 2.0, h: tl.h + 8.0 }), rgba(0xfcf3a4)));
                        // Age legend ramp.
                        let lx = panel_x + 12.0;
                        let ly = HEADER_H + 30.0;
                        let lw = PANEL_W - 24.0;
                        for i in 0..lw as usize {
                            let f = i as f64 / lw as f64;
                            let col = age_color(20.0 * f);
                            window.paint_quad(gpui::fill(bounds(Rect { x: lx + i as f32, y: ly, w: 1.5, h: 8.0 }), col));
                        }
                    },
                )
                .absolute()
                .size_full(),
            )
            .children(labels)
            .children(years)
            // Header: date and totals, then breadcrumb and hover info.
            .child(
                div()
                    .absolute()
                    .left(px(PAD))
                    .top(px(8.0))
                    .w(px(vw - 2.0 * PAD))
                    .flex()
                    .flex_col()
                    .gap_1()
                    .child(
                        div()
                            .flex()
                            .items_baseline()
                            .gap_4()
                            .text_color(rgba(0xe4e4e7))
                            .child(div().text_size(px(22.0)).child(date(r.data.meta.commits[c].time)))
                            .child(div().text_size(px(13.0)).text_color(rgba(0xa1a1aa)).child(format!(
                                "{} live lines  ·  commit {}  ·  {}/{}",
                                thousands(total),
                                &r.data.meta.commits[c].hash[..12],
                                thousands(c as i64 + 1),
                                thousands(n as i64)
                            )))
                            .child(div().flex_1())
                            .child(div().text_size(px(11.0)).text_color(rgba(0x71717a)).child(if r.fps > 0.0 {
                                format!("{:.0} fps", r.fps)
                            } else {
                                "space: play  ·  click: zoom  ·  esc: back".to_string()
                            })),
                    )
                    .child(
                        div()
                            .flex()
                            .gap_4()
                            .text_size(px(12.0))
                            .text_color(rgba(0x71717a))
                            .child(div().flex().children(crumbs))
                            .child(div().flex_1())
                            .child(div().text_color(rgba(0xa1a1aa)).child(info)),
                    ),
            )
            // Right panel: age legend, then provenance of the current view.
            .child(
                div()
                    .absolute()
                    .left(px(panel_x))
                    .top(px(HEADER_H))
                    .w(px(PANEL_W))
                    .h(px(vh - HEADER_H - TIMELINE_H))
                    .p(px(12.0))
                    .bg(rgba(0x18181c))
                    .flex()
                    .flex_col()
                    .gap_2()
                    .text_color(rgba(0xa1a1aa))
                    .child(div().text_size(px(11.0)).child(match sel {
                        Some(f) => format!("Lines from {}: their mean age", r.data.src_names[f as usize]),
                        None => "Mean age of lines".to_string(),
                    }))
                    .child(div().h(px(8.0)))
                    .child(
                        div().relative().h(px(12.0)).text_size(px(10.0)).children(legend_ticks.iter().map(|&y| {
                            div()
                                .absolute()
                                .left(px((PANEL_W - 24.0) * (y / 20.0) as f32 - if y > 0.0 { 14.0 } else { 0.0 }))
                                .child(if y >= 20.0 { "20 y+".to_string() } else { format!("{y:.0}") })
                        })),
                    )
                    .child(div().h(px(14.0)))
                    .child(div().text_size(px(11.0)).child(if stale {
                        "Where live lines came from (computing…)".to_string()
                    } else {
                        "Where live lines came from".to_string()
                    }))
                    .child(
                        div()
                            .text_size(px(10.0))
                            .text_color(rgba(0x71717a))
                            .child("The maintainer tree or author that brought them into the mainline. Click one to see its territory."),
                    )
                    .children(prov_rows),
            );
        let done = r.record_frame(t0.elapsed(), bench);
        if done {
            cx.quit();
        }
        el.into_any_element()
    }
}
