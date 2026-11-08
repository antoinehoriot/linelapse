//! `linelapse --record out.mp4`: render the playback offscreen and encode it.
//!
//! Decision: GPUI has no API to read back a rendered frame (only OS screen capture, which
//! needs a permission), so the recorder redraws the same scene with a software
//! rasterizer (tiny-skia + ab_glyph). Tile layout, colours, provenance rows and cohort
//! bands come from the same functions the window uses, so the video matches the app.
//! Frames are piped to ffmpeg as raw RGBA; without ffmpeg, a directory of PNGs is written.

use std::io::Write;
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::Instant;

use ab_glyph::{Font, FontRef, PxScale, ScaleFont, point as gpoint};
use gpui::Hsla;
use tiny_skia::{FillRule, Paint, PathBuilder, Pixmap, Rect as SkRect, Transform};

use crate::app::{
    Data, HEADER_H, PAD, PANEL_W, age_color, cohort_polygons, date, layout_tiles, map_rect,
    provenance_rows, thousands, timeline_rect,
};
use crate::treemap::Rect;

pub struct RecordOpts {
    pub out: String,
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    pub seconds: f64,
    /// Fraction of history to start from.
    pub from: f64,
    pub view: usize,
    pub focus: Option<u32>,
    /// Half-life of the view, computed once before recording.
    pub half_life: Option<Option<f64>>,
}

/// Logical layout size: the window's default, scaled to the output height.
const LOGICAL_H: f32 = 900.0;

struct Canvas<'f> {
    pix: Pixmap,
    scale: f32,
    font: FontRef<'f>,
}

fn sk_color(c: Hsla, alpha: f32) -> tiny_skia::Color {
    let c = c.to_rgb();
    tiny_skia::Color::from_rgba(c.r, c.g, c.b, c.a * alpha).unwrap_or(tiny_skia::Color::BLACK)
}

fn hex(h: u32) -> Hsla {
    gpui::rgb(h).into()
}

impl Canvas<'_> {
    fn rect(&mut self, r: Rect, c: Hsla, alpha: f32) {
        let s = self.scale;
        if let Some(rect) = SkRect::from_xywh(r.x * s, r.y * s, r.w * s, r.h * s) {
            let mut p = Paint::default();
            p.set_color(sk_color(c, alpha));
            p.anti_alias = false;
            self.pix.fill_rect(rect, &p, Transform::identity(), None);
        }
    }

    fn polygon(&mut self, pts: &[(f32, f32)], c: Hsla) {
        let s = self.scale;
        let mut b = PathBuilder::new();
        b.move_to(pts[0].0 * s, pts[0].1 * s);
        for &(x, y) in &pts[1..] {
            b.line_to(x * s, y * s);
        }
        b.close();
        if let Some(path) = b.finish() {
            let mut p = Paint::default();
            p.set_color(sk_color(c, 1.0));
            p.anti_alias = true;
            self.pix
                .fill_path(&path, &p, FillRule::Winding, Transform::identity(), None);
        }
    }

    /// Draw `text` with its top-left at (x, y) logical px, clipped to `max_w`.
    fn text(&mut self, text: &str, x: f32, y: f32, size: f32, c: Hsla, max_w: f32) {
        let s = self.scale;
        let scale = PxScale::from(size * s);
        let sf = self.font.as_scaled(scale);
        let (x0, base) = (x * s, y * s + sf.ascent());
        let limit = x0 + max_w * s;
        let rgb = c.to_rgb();
        let (w, h) = (self.pix.width() as i32, self.pix.height() as i32);
        let mut caret = x0;
        let mut prev = None;
        for ch in text.chars() {
            let id = self.font.glyph_id(ch);
            if let Some(p) = prev {
                caret += sf.kern(p, id);
            }
            let adv = sf.h_advance(id);
            if caret + adv > limit {
                break;
            }
            let glyph = id.with_scale_and_position(scale, gpoint(caret, base));
            if let Some(og) = self.font.outline_glyph(glyph) {
                let bb = og.px_bounds();
                let data = self.pix.data_mut();
                og.draw(|gx, gy, cov| {
                    let (px, py) = (bb.min.x as i32 + gx as i32, bb.min.y as i32 + gy as i32);
                    if px < 0 || py < 0 || px >= w || py >= h || cov <= 0.0 {
                        return;
                    }
                    let i = 4 * (py * w + px) as usize;
                    // Opaque background, so premultiplied == straight: plain lerp.
                    for (k, v) in [rgb.r, rgb.g, rgb.b].into_iter().enumerate() {
                        let d = data[i + k] as f32;
                        data[i + k] = (d + (v * 255.0 - d) * cov.min(1.0)) as u8;
                    }
                });
            }
            caret += adv;
            prev = Some(id);
        }
    }
}

fn draw_frame(cv: &mut Canvas, data: &Data, o: &RecordOpts, vw: f32, vh: f32) {
    let e = &data.engine;
    let n = e.len();
    let c = e.current();
    cv.pix.fill(sk_color(hex(0x111114), 1.0));

    // Map.
    let map = map_rect(vw, vh);
    let tiles = layout_tiles(data, o.view, o.focus, map);
    for t in &tiles {
        cv.rect(t.rect, t.color, 1.0);
    }
    for t in &tiles {
        if let Some(label) = &t.label {
            let col = if t.frame {
                hex(0xd4d4d8)
            } else {
                hex(0x0b0b0e).opacity(0.75)
            };
            let size = if t.frame { 11.0 } else { 10.0 };
            cv.text(
                label,
                t.rect.x + 4.0,
                t.rect.y + 2.0,
                size,
                col,
                t.rect.w - 8.0,
            );
        }
    }

    // Header.
    let node = &data.tree.nodes[o.view];
    let total = e.agg(node.lo as usize, node.hi as usize).count;
    cv.text(
        &date(data.meta.commits[c].time),
        PAD,
        8.0,
        22.0,
        hex(0xe4e4e7),
        200.0,
    );
    let repo = Path::new(&data.meta.repo)
        .file_name()
        .map_or("repo".into(), |s| {
            s.to_string_lossy().trim_end_matches(".git").to_string()
        });
    let path = if node.path.is_empty() {
        repo
    } else {
        format!("{repo} / {}", node.path)
    };
    cv.text(
        &format!("{}  ·  {} live lines", path, thousands(total)),
        PAD + 140.0,
        16.0,
        13.0,
        hex(0xa1a1aa),
        vw - 2.0 * PAD - 300.0,
    );
    cv.text(
        "linelapse",
        vw - PAD - 60.0,
        14.0,
        12.0,
        hex(0x71717a),
        80.0,
    );

    // Side panel: legend and provenance.
    let px0 = vw - PAD - PANEL_W;
    cv.rect(
        Rect {
            x: px0,
            y: HEADER_H,
            w: PANEL_W,
            h: map.h,
        },
        hex(0x18181c),
        1.0,
    );
    let title = match o.focus {
        Some(f) => format!("Lines from {}: their mean age", data.src_names[f as usize]),
        None => "Mean age of lines".to_string(),
    };
    cv.text(
        &title,
        px0 + 12.0,
        HEADER_H + 12.0,
        11.0,
        hex(0xa1a1aa),
        PANEL_W - 24.0,
    );
    let lw = PANEL_W - 24.0;
    for i in 0..lw as usize {
        let col = age_color(20.0 * i as f64 / lw as f64);
        cv.rect(
            Rect {
                x: px0 + 12.0 + i as f32,
                y: HEADER_H + 30.0,
                w: 1.5,
                h: 8.0,
            },
            col,
            1.0,
        );
    }
    for y in [0.0f32, 5.0, 10.0, 15.0, 20.0] {
        let label = if y >= 20.0 {
            "20 y+".to_string()
        } else {
            format!("{y:.0}")
        };
        let x = px0 + 12.0 + lw * y / 20.0 - if y > 0.0 { 14.0 } else { 0.0 };
        cv.text(&label, x, HEADER_H + 42.0, 10.0, hex(0xa1a1aa), 40.0);
    }
    cv.text(
        &crate::app::half_life_text(o.half_life),
        px0 + 12.0,
        HEADER_H + 58.0,
        12.0,
        hex(0xe4e4e7),
        PANEL_W - 24.0,
    );
    cv.text(
        "Where live lines came from",
        px0 + 12.0,
        HEADER_H + 80.0,
        11.0,
        hex(0xa1a1aa),
        PANEL_W - 24.0,
    );
    if let Some(by_src) = e.provenance() {
        let rows = provenance_rows(data, o.focus, by_src);
        let sum: i64 = rows.iter().map(|r| r.2).sum::<i64>().max(1);
        let mut y = HEADER_H + 100.0;
        for (src, name, k) in rows {
            if y > HEADER_H + map.h - 30.0 {
                break;
            }
            let f = k as f32 / sum as f32;
            let sel = src.is_some() && src == o.focus;
            let col = if sel {
                hex(crate::app::TERRITORY)
            } else {
                hex(0xd4d4d8)
            };
            cv.text(&name, px0 + 16.0, y, 11.0, col, PANEL_W - 90.0);
            cv.text(
                &format!("{:.1}%", f * 100.0),
                px0 + PANEL_W - 56.0,
                y,
                11.0,
                hex(0xa1a1aa),
                50.0,
            );
            let bar = if sel {
                hex(crate::app::TERRITORY)
            } else {
                hex(0xa1a1aa)
            };
            cv.rect(
                Rect {
                    x: px0 + 16.0,
                    y: y + 16.0,
                    w: (PANEL_W - 32.0) * f,
                    h: 3.0,
                },
                bar,
                1.0,
            );
            y += 30.0;
        }
    }

    // Timeline: cohort bands, dimmed future, moments, playhead, years.
    let tl = timeline_rect(vw, vh);
    cv.rect(tl, hex(0x1c1c21), 1.0);
    for (pts, col) in cohort_polygons(e, tl) {
        cv.polygon(&pts, col);
    }
    let x_of = |k: usize| tl.x + tl.w * k as f32 / (n - 1).max(1) as f32;
    let head = x_of(c);
    cv.rect(
        Rect {
            x: head,
            y: tl.y,
            w: (tl.x + tl.w - head).max(0.0),
            h: tl.h,
        },
        hex(0x1c1c21),
        0.7,
    );
    for m in &data.moments {
        let x = x_of(m.commit);
        cv.rect(
            Rect {
                x: x - 3.0,
                y: tl.y - 12.0,
                w: 6.0,
                h: 6.0,
            },
            hex(0xa1a1aa),
            1.0,
        );
    }
    cv.rect(
        Rect {
            x: head - 1.0,
            y: tl.y - 4.0,
            w: 2.0,
            h: tl.h + 8.0,
        },
        hex(0xfcf3a4),
        1.0,
    );
    let years = &e.cohort_years;
    let step = ((years.len() as i32 - 1) / 8).max(1);
    for &y in years.iter().skip(1) {
        if (y - years[0]) % step != 0 {
            continue;
        }
        let ts = (y as i64 - 1970) as f64 * 365.2425 * 86400.0;
        let i = data
            .meta
            .commits
            .partition_point(|cm| (cm.time as f64) < ts);
        if i > 0 && i < n {
            cv.text(
                &y.to_string(),
                x_of(i) - 12.0,
                tl.y + tl.h + 2.0,
                10.0,
                hex(0x71717a),
                40.0,
            );
        }
    }
}

/// Spawn ffmpeg reading raw RGBA frames on stdin. `None` if ffmpeg is not installed.
fn ffmpeg(o: &RecordOpts) -> Option<std::process::Child> {
    let size = format!("{}x{}", o.width, o.height);
    let fps = o.fps.to_string();
    let mut cmd = Command::new("ffmpeg");
    cmd.args([
        "-y",
        "-loglevel",
        "error",
        "-f",
        "rawvideo",
        "-pix_fmt",
        "rgba",
    ])
    .args(["-s", &size, "-r", &fps, "-i", "-"]);
    if o.out.ends_with(".gif") {
        let w = (o.width / 2).to_string();
        let vf = format!(
            "fps=30,scale={w}:-1:flags=lanczos,split[a][b];[a]palettegen=max_colors=128[p];[b][p]paletteuse=dither=sierra2_4a"
        );
        cmd.args(["-vf", &vf]);
    } else {
        cmd.args([
            "-c:v", "libx264", "-pix_fmt", "yuv420p", "-crf", "18", "-preset", "medium",
        ]);
        cmd.args(["-movflags", "+faststart"]);
    }
    cmd.arg(&o.out).stdin(Stdio::piped()).spawn().ok()
}

pub fn run(data: &mut Data, o: &RecordOpts) -> Result<(), String> {
    let font = FontRef::try_from_slice(epaint_default_fonts::UBUNTU_LIGHT)
        .map_err(|e| format!("embedded font: {e}"))?;
    let scale = o.height as f32 / LOGICAL_H;
    let (vw, vh) = (o.width as f32 / scale, LOGICAL_H);
    let pix = Pixmap::new(o.width, o.height).ok_or("bad output size")?;
    let mut cv = Canvas { pix, scale, font };

    let n = data.engine.len();
    let start = (o.from.clamp(0.0, 1.0) * (n - 1) as f64) as usize;
    let play = (o.seconds * o.fps as f64).round().max(2.0) as usize;
    let hold = (1.5 * o.fps as f64) as usize; // linger on the last frame
    let frames = play + hold;

    let to_pngs = !o.out.contains('.') || o.out.ends_with('/');
    let mut child = if to_pngs { None } else { ffmpeg(o) };
    if !to_pngs && child.is_none() {
        return Err("ffmpeg not found: install it, or pass a directory to get PNG frames".into());
    }
    if to_pngs {
        std::fs::create_dir_all(&o.out).map_err(|e| e.to_string())?;
    }
    let node = &data.tree.nodes[o.view];
    let (lo, hi) = (node.lo, node.hi);
    let t0 = Instant::now();
    let mut last_report = Instant::now();
    for f in 0..frames {
        let k = f.min(play - 1);
        let c = start + (n - 1 - start) * k / (play - 1);
        data.engine.seek(c);
        data.engine.watch_provenance(lo, hi);
        data.engine.watch_territory(o.focus);
        data.engine.sync();
        draw_frame(&mut cv, data, o, vw, vh);
        match &mut child {
            Some(ch) => ch
                .stdin
                .as_mut()
                .ok_or("ffmpeg stdin")?
                .write_all(cv.pix.data())
                .map_err(|e| format!("ffmpeg: {e}"))?,
            None => cv
                .pix
                .save_png(Path::new(&o.out).join(format!("{f:05}.png")))
                .map_err(|e| e.to_string())?,
        }
        if last_report.elapsed().as_secs_f64() > 1.0 {
            eprintln!(
                "frame {}/{} ({:.0} fps)",
                f + 1,
                frames,
                f as f64 / t0.elapsed().as_secs_f64()
            );
            last_report = Instant::now();
        }
    }
    if let Some(mut ch) = child {
        drop(ch.stdin.take());
        let st = ch.wait().map_err(|e| e.to_string())?;
        if !st.success() {
            return Err(format!("ffmpeg failed ({st})"));
        }
    }
    eprintln!(
        "wrote {} ({} frames in {:.1}s)",
        o.out,
        frames,
        t0.elapsed().as_secs_f64()
    );
    Ok(())
}
