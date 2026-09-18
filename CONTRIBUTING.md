# Contributing to anacraft

Thanks for looking. anacraft is a small Rust binary — a terminal dashboard for
Google Analytics 4 — so the loop is short: clone, run the demo, change a panel,
send a PR.

## Getting set up

```sh
git clone https://github.com/mehfuzh/anacraft.git
cd anacraft
cargo run -- dash --demo
```

Rust 1.74+ and a truecolor terminal are the only requirements.

**You do not need a Google account to work on this.** `--demo` drives the whole
dashboard from synthetic data, which is how most of the UI work gets done. The
generator lives in `src/ui.rs` and produces plausible traffic, realtime events
and country splits, so panels animate exactly as they do against real data.

If you do want to point it at a real property, `anacraft login` needs an OAuth
client. There is no secret baked into the source; supply your own, either at
build time or at runtime:

```sh
export ANACRAFT_OAUTH_CLIENT_ID=...
export ANACRAFT_OAUTH_CLIENT_SECRET=...
```

## Before you open a PR

CI runs exactly three things, and all three must be green:

```sh
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test
```

Run them locally first — `cargo fmt` (no flag) fixes the first one for you.
Clippy is set to deny warnings, so a lint is a build failure, not a suggestion.

## How the code is laid out

| File | What lives there |
|------|------------------|
| `src/main.rs` | CLI surface — subcommands, global flags, one-shot reports |
| `src/ui.rs` | The dashboard: layout, panels, animation, demo data |
| `src/ga.rs` | GA4 Data API client and the report request shapes |
| `src/mcp.rs` | `craft mcp` — the MCP server, its tools, and its synthetic answers |
| `src/auth.rs` | OAuth sign-in — the loopback redirect listener and token refresh |
| `src/theme.rs` | Palettes, the ore vocabulary, glyphs, metric definitions |
| `src/render.rs` | Shared drawing helpers — bars, deltas, sparklines |
| `src/achievements.rs` | The milestone rules behind the toasts |
| `src/config.rs` | `~/.anacraft/config.json` and `token.json` |
| `docs/` | The anacraft.dev site, served by GitHub Pages |

### A note on the ore vocabulary

Metrics wear Minecraft clothes — users are VILLAGERS, page views are BLOCKS
MINED, bounce rate is CREEPER RATE. Each one keeps its plain GA4 name visible
beside it, so the dashboard stays readable by someone who just wants their
numbers. Please keep both halves when you add a metric; the joke should never
cost anyone the meaning.

Colors go through `src/theme.rs` rather than being written inline. Reach for an
ore name (`ore::diamond()`, `ore::redstone()`) instead of a literal, so whatever
you add survives a theme swap across all seven palettes.

## Testing UI code

The dashboard is hard to assert against wholesale, so the tests target pure
helpers instead — bar widths, easing, chart fitting, header budgeting. If you
touch layout, the useful question is usually "can this overflow its panel?", and
the answer belongs in a test that sweeps a range of widths. `the_daily_chart_fits_its_panel`
and `header_realms_are_dropped_whole_never_sliced` are the pattern to copy.

The `src/render.rs` helpers emit no color under `cfg(test)`, so assertions can compare
bare text and count characters directly.

## Themes

A new palette is a `Palette` const in `src/theme.rs` added to the `THEMES`
array. Fill in every field — the array is what `t` cycles through and what
`anacraft theme` lists, and a missing shade shows up as a hole in some panel you
were not looking at.

## The site

`docs/` is the published site. The embedded dashboard on it is a real capture,
not a mockup — the dashboard rendered into a buffer and converted to markup one
cell at a time, for every palette, at both the wide and the phone-sized reflow.

If a change alters how the dashboard looks, the capture is stale. Regenerate it:

```sh
make capture
```

That runs the hidden `anacraft capture` subcommand and splices its output into
`docs/index.html` between the `<!-- capture:start -->` markers. The demo data is
seeded and the footer clock is frozen, so regenerating with nothing changed is a
no-op diff — if `git diff` is noisy, the dashboard really did change.

Please run it in the same PR as the change. These used to be maintained by hand,
which is how the site came to advertise a panel the dashboard no longer drew.

## Releases

Maintainers only. A release is a workflow dispatch: run **release** from the
Actions tab (or `gh workflow run release.yml -f bump=minor`) and it bumps the
crate and the site's footers, regenerates the captures, commits `Release
vX.Y.Z`, and tags it. `bump` takes `patch`, `minor`, `major`, or an explicit
`x.y.z`, and defaults to `minor`.

Pushing a `v*` tag by hand still works — it skips the bump, for when the version
commit already exists.

Either way `release.yml` builds macOS (arm64, x86_64), Linux musl (arm64,
x86_64) and Windows x86_64, then publishes the archives with checksums. Release
notes are written by Copilot from the PRs merged since the last tag; work pushed
straight to main falls back to GitHub's generated notes. Assets are named
`anacraft-<tag>-<target>`, and `install.sh` resolves the tag from the GitHub API
— so if that naming ever changes, the installer has to change with it.

## Reporting bugs

Terminal bugs are mostly about size and palette, so please include your terminal
emulator, its dimensions, and the output of `anacraft theme`. If the dashboard
drew something wrong, a paste of the broken frame is worth more than a
description of it.

## License

Contributions are accepted under the [Apache License 2.0](LICENSE).
