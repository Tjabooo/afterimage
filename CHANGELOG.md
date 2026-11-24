# Changelog

All notable changes to this project are listed here ([Keep a Changelog](https://keepachangelog.com), [SemVer](https://semver.org)).

## [Unreleased]

## [0.1.0] - 2026-11-24

First release.

- Recorder daemon: wlr-screencopy every 2 s, 2×2 overlapping tiles per output, OCR only for tiles that changed and
  settled, frames saved when the text on screen changed, privacy blackouts, pause for games/lock screen/by hand.
- SQLite FTS5 index of text spans and links; question-style search with relative times and `app:` filters.
- GTK4 timeline scrubber (ember palette): frames, activity timeline, results/links, spotlight on the matched tile.
- Retention by age and size; waybar status module; systemd user service; `install.sh`.
