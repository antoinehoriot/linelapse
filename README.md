# linelapse

**Replay a git repository's history, line by line.** Every directory is a tile sized by its live lines and coloured by their mean age. Drag through twenty years of history at 120 fps, zoom into any directory, and see which maintainer trees brought the code in.

![Linux, 2005 to 2026, in 20 seconds](docs/demo.gif)

*The Linux kernel from v2.6.12 (2005) to November 2026: yellow is fresh code, violet is old. Recorded with `linelapse --record`.*

## Numbers, on the Linux kernel

77,605 mainline commits, 172,609 files ever, 92 million lines ever written, on a 14-core Mac:

| | |
|---|---|
| Full indexing | 32 s |
| Cache on disk | 126 MB |
| Playback | 120 fps, frame p95 under 10 ms |
| Jump to any commit | under 0.3 ms |
| Accuracy | 0 mismatches over 97,316 files, checked against git at 4 commits (`--check`) |
| 20 s video at 1080p | rendered in 6 s |

## What you see

![linelapse on Linux at HEAD](docs/hero.avif)

- **The map.** A treemap of the repository at the current commit. Tiles keep their place while you scrub, so you watch directories grow, shrink and age.
- **The timeline.** Live lines stacked by the year they were written. Hover it to see how much of the code still dates from the first year.
- **Key moments.** Markers on the commits that changed the most lines. Click one to jump there.
- **Where code came from.** The side panel ranks the sources of the live lines in view: the maintainer tree named in each merge (`netdev/net-next`, `drm/drm`...), the merged branch, or the author of a direct commit. It updates live during playback.
- **A source's territory.** Click a source: tiles keep the age colour of that source's own lines and fade where it owns less. Its commits light up on the timeline.
- **Half-life.** How long it took for half of the lines ever written in the current view to disappear (Kaplan-Meier, with lines still alive censored at HEAD). Linux: 6.8 years. `drivers/gpu`: 14.6 years.
- **Languages.** Press `l` to colour tiles by the language with the most live lines.

### Modes

| A source's territory | Languages | Zoom on a subsystem |
|---|---|---|
| ![Territory of davem/net-next](docs/territory.gif) | ![Colour by language](docs/languages.gif) | ![Zoom on drivers/gpu](docs/zoom.gif) |
| `--source davem/net-next`: the networking tree owns `drivers/net` and `net/`; everything else fades. | `--color language` (or `l`): Devicetree takes over `arch/arm*`, Rust appears in `rust/`. | `--path drivers/gpu`: `amd/` grows fresh and yellow while `radeon/` turns violet. |

## Install

```sh
cargo install --git https://github.com/antoinehoriot/linelapse
```

Needs `git` on the PATH, and `ffmpeg` only for MP4/GIF export. Tested on macOS; GPUI is built with its `runtime_shaders` feature, so the Xcode Metal Toolchain is not required.

## Usage

```sh
linelapse https://github.com/torvalds/linux    # clones into the cache, indexes, opens
linelapse ~/code/my-repo                       # a local repository
linelapse . --at 0.3 --path src                # 30% into history, zoomed on src/
linelapse . --source netdev/net                # open on a source's territory
linelapse . --color language                   # open in language mode
linelapse . --check                            # verify the index against git, no window
linelapse . --rebuild                          # rebuild the cache
```

Keys: space play/pause, ← → one commit (shift: 500), `l` age/language, esc or backspace to go back. Click a directory to zoom in.

The cache lives in `~/Library/Caches/linelapse/` (macOS) or `~/.cache/linelapse/`, and is rebuilt when HEAD changes. Remote repositories are bare-cloned under `repos/` there and fetched on later runs.

### Record a video

```sh
linelapse ~/code/linux --record linux.mp4                        # 1920x1080, 60 fps, 20 s
linelapse ~/code/linux --record linux.gif --size 1280x720 --seconds 12
linelapse . --record frames/ --source netdev/net --at 0.5        # PNG frames, from mid-history
linelapse . --record langs.mp4 --color language
```

Videos are rendered offscreen, with no window and no screen-recording permission, using the same layout, colours and panel as the app. MP4 and GIF need `ffmpeg`; a directory gets PNG frames.

## How it works

1. One `git log -p -U0 --first-parent` process per core, each on a slice of history.
2. Every line gets a birth commit and a death commit. Events are run-length encoded per (commit, file).
3. Files are numbered in path order, so every directory is a contiguous id range.
4. The viewer keeps per-file state with checkpoints: a jump replays a few hundred commits at most.
5. The side panel and the territory view are live trackers updated on every step. Full rescans (new view, new source, long jump) run on a background thread and catch up with the playhead when they land, so they never block a frame.

## Prior art

linelapse stands on ideas from tools worth knowing:

- [Gource](https://github.com/acaudwell/Gource) animates who touched which files, as a growing tree. linelapse shows the state of the code (age, origin) rather than activity, and lets you jump anywhere instead of playing forward.
- [git-of-theseus](https://github.com/erikbern/git-of-theseus) and [Hercules](https://github.com/src-d/hercules) chart how code ages over time (cohorts, survival, burndown). The cohort timeline and the half-life come from them; linelapse makes them interactive and spatial.
- [Git Truck](https://github.com/git-truck/git-truck) maps a repository's structure and authorship in the browser.

## Known limits

- Lines are dated by when they reached the mainline (often a merge), not by their author's commit.
- Renames are not followed (a rename is a deletion plus a creation). Binary files and submodules are ignored.
- The cache is rebuilt from scratch when HEAD changes (not incremental yet).
- Only tested on macOS so far.

## License

MIT. Videos embed the Ubuntu Light font (Ubuntu Font Licence 1.0) via `epaint_default_fonts`.
