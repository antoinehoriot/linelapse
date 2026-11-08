//! linelapse: replay a git repository's history, line by line.
//!
//! Usage: linelapse [repo] [--rebuild] [--check] [--bench] [--at <0..1>] [--path <dir>]
//!              [--source <name>]
//!        linelapse [repo] --record <out.mp4|out.gif|dir/> [--size 1920x1080] [--fps 60]
//!              [--seconds 20] [--at <0..1>] [--path <dir>] [--source <name>]
//!              [--color age|language]
//! Keys: space play/pause, left/right one commit (shift: 500), escape/backspace up.
//! Click a directory to zoom in; click a marker on the timeline to jump to that moment;
//! click a source in the provenance panel to see its territory.

mod app;
mod build;
mod check;
mod couple;
mod engine;
mod lang;
mod record;
mod remote;
mod story;
mod tree;
mod treemap;

use std::path::PathBuf;
use std::sync::Mutex;

use gpui::{App, Application, TitlebarOptions, WindowBounds, WindowOptions, prelude::*, px, size};

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let flag = |f: &str| args.iter().any(|a| a == f);
    let value = |f: &str| {
        args.iter()
            .position(|a| a == f)
            .and_then(|i| args.get(i + 1))
            .cloned()
    };
    let mut skip_next = false;
    let mut positional = None;
    for a in &args {
        if skip_next {
            skip_next = false;
        } else if [
            "--at",
            "--path",
            "--source",
            "--record",
            "--size",
            "--fps",
            "--seconds",
            "--color",
        ]
        .contains(&a.as_str())
        {
            skip_next = true;
        } else if !a.starts_with("--") {
            positional = Some(a.clone());
        }
    }
    let repo: String = positional.unwrap_or_else(|| ".".into());
    let rebuild = flag("--rebuild");

    if flag("--check") {
        let progress = build::Progress::default();
        let phase = Mutex::new(String::new());
        let code = match app::load_or_build(&repo, rebuild, &progress, &phase).and_then(|mut d| {
            let path = std::path::PathBuf::from(&d.meta.repo);
            let k = check::run(&path, &mut d)?;
            // Birth dates vs git blame: reported, not yet part of the pass/fail verdict.
            check::blame(&path, &d, 300)?;
            Ok(k)
        }) {
            Ok(0) => {
                println!("OK: 0 mismatches");
                0
            }
            Ok(k) => {
                println!("FAILED: {k} mismatches");
                1
            }
            Err(e) => {
                eprintln!("error: {e}");
                2
            }
        };
        std::process::exit(code);
    }

    if let Some(out) = value("--record") {
        let progress = build::Progress::default();
        let phase = Mutex::new(String::new());
        let (w, h) = value("--size")
            .and_then(|s| {
                let (w, h) = s.split_once('x')?;
                Some((w.parse().ok()?, h.parse().ok()?))
            })
            .unwrap_or((1920u32, 1080u32));
        let result = app::load_or_build(&repo, rebuild, &progress, &phase).and_then(|mut d| {
            let view = value("--path").and_then(|p| d.tree.find(&p)).unwrap_or(0);
            let focus = match value("--source") {
                Some(name) => Some(
                    d.src_names
                        .iter()
                        .position(|s| *s == name)
                        .ok_or(format!("unknown source: {name}"))? as u32,
                ),
                None => None,
            };
            let o = record::RecordOpts {
                out,
                // Even sizes: yuv420p needs them.
                width: w.max(320) & !1,
                height: h.max(240) & !1,
                fps: value("--fps").and_then(|v| v.parse().ok()).unwrap_or(60),
                seconds: value("--seconds")
                    .and_then(|v| v.parse().ok())
                    .unwrap_or(20.0),
                from: value("--at").and_then(|v| v.parse().ok()).unwrap_or(0.0),
                view,
                focus,
                mode: match value("--color").as_deref() {
                    Some("language") | Some("lang") => app::ColorMode::Language,
                    _ => app::ColorMode::Age,
                },
                half_life: Some({
                    let node = &d.tree.nodes[view];
                    (d.engine.half_life_job(node.lo, node.hi))()
                }),
            };
            record::run(&mut d, &o)
        });
        if let Err(e) = result {
            eprintln!("error: {e}");
            std::process::exit(2);
        }
        return;
    }

    let opts = app::Opts {
        bench: flag("--bench"),
        at: value("--at").and_then(|v| v.parse().ok()),
        start_path: value("--path"),
        source: value("--source"),
        mode: match value("--color").as_deref() {
            Some("language") | Some("lang") => app::ColorMode::Language,
            _ => app::ColorMode::Age,
        },
    };
    let title = format!(
        "linelapse · {}",
        std::path::Path::new(&repo)
            .canonicalize()
            .unwrap_or_else(|_| PathBuf::from(&repo))
            .file_name()
            .map(|n| n.to_string_lossy().trim_end_matches(".git").to_string())
            .unwrap_or_else(|| "repo".into())
    );
    Application::new().run(move |cx: &mut App| {
        let wopts = WindowOptions {
            window_bounds: Some(WindowBounds::centered(size(px(1440.), px(900.)), cx)),
            titlebar: Some(TitlebarOptions {
                title: Some(title.into()),
                ..Default::default()
            }),
            focus: true,
            ..Default::default()
        };
        cx.open_window(wopts, |window, cx| {
            cx.new(|cx| {
                let s = app::Scrubber::new(repo, rebuild, opts, cx);
                s.focus_handle().focus(window);
                s
            })
        })
        .expect("open window");
        cx.on_window_closed(|cx| cx.quit()).detach();
        cx.activate(true);
    });
}
