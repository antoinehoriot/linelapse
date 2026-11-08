# linelapse

Replay a git repository's history, line by line. Every directory is sized by its live lines and coloured by their mean age (yellow = fresh, violet = old). Drag the timeline to travel through time, click to zoom in, and see where the code came from.

On the Linux kernel (77,605 mainline commits, 2005 to 2026):

| | |
|---|---|
| Full indexing | 32 s (14 cores) |
| Cache | 126 MB |
| Playback | 120 fps, frame p95 < 10 ms, seek < 0.3 ms |
| Accuracy | 0 mismatches over 97,316 files, checked against git at 4 commits |

## Usage

```sh
cargo build --release
./target/release/linelapse ~/code/my-repo                 # indexes on first run, then opens
./target/release/linelapse . --at 0.3 --path src          # 30% into history, zoomed on src/
./target/release/linelapse . --source netdev/net          # open on a source's territory
./target/release/linelapse . --check                      # verify the index against git, no window
./target/release/linelapse . --rebuild                    # rebuild the cache
./target/release/linelapse . --bench                      # full playback, timings on stderr
```

Keys: space play/pause, ← → one commit (shift: 500), esc or backspace to go back.

- **Click a directory** to zoom in.
- **Click a marker** on the timeline to jump to that moment (the commits that changed the most lines).
- **Click a source** in the side panel to see its territory: tiles keep the age colour of that source's own lines and fade where it owns less. Its commits light up on the timeline.

The cache lives in `~/Library/Caches/linelapse/` (macOS) or `~/.cache/linelapse/`, and is rebuilt when HEAD changes.

## How it works

1. One `git log -p -U0 --first-parent` process per core, each on a slice of history.
2. Every line gets a birth commit and a death commit. Events are run-length encoded per (commit, file).
3. Files are numbered in path order, so every directory is a contiguous id range.
4. The viewer keeps per-file state with checkpoints, so a seek replays a few hundred commits at most.
5. The side panel and the territory view are live trackers updated on every step. Full rescans (a new view, a new source, a long jump) run on a background thread and catch up with the playhead when they land, so they never block a frame.

## Known limits

- Lines are dated by when they reached the mainline (often a merge), not by their author's commit.
- "Source" is the maintainer tree named in the merge message, the merged branch, or the author of a direct commit.
- Renames are not followed (a rename is a deletion plus a creation). Binary files and submodules are ignored.
- The cache is rebuilt from scratch when HEAD changes (not incremental yet).
- Tested on macOS only. GPUI is built with the `runtime_shaders` feature, so the Xcode Metal Toolchain is not required.

## License

MIT
