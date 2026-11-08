//! linelapse: replay a git repository's history, line by line.
//!
//! Usage: linelapse [repo] [--rebuild] [--check] [--bench] [--at <0..1>] [--path <dir>]
//!              [--source <name>]
//! Keys: space play/pause, left/right one commit (shift: 500), escape/backspace up.
//! Click a directory to zoom in; click a marker on the timeline to jump to that moment;
//! click a source in the provenance panel to see its territory.

mod app;
mod build;
mod check;
mod engine;
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
        } else if a == "--at" || a == "--path" || a == "--source" {
            skip_next = true;
        } else if !a.starts_with("--") {
            positional = Some(a.clone());
        }
    }
    let repo = PathBuf::from(positional.unwrap_or_else(|| ".".into()));
    let rebuild = flag("--rebuild");

    if flag("--check") {
        let progress = build::Progress::default();
        let phase = Mutex::new(String::new());
        let code = match app::load_or_build(&repo, rebuild, &progress, &phase)
            .and_then(|mut d| check::run(&repo, &mut d))
        {
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

    let opts = app::Opts {
        bench: flag("--bench"),
        at: value("--at").and_then(|v| v.parse().ok()),
        start_path: value("--path"),
        source: value("--source"),
    };
    let title = format!(
        "linelapse · {}",
        repo.canonicalize()
            .ok()
            .and_then(|p| p.file_name().map(|n| n.to_string_lossy().to_string()))
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
