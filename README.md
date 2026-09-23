<h1 align="center">
<img width="300px" src="assets/logo.png" />

[![CI][s0]][l0] [![crates][s1]][l1] ![MIT][s2] [![UNSAFE][s3]][l3] [![TWEET][s6]][l6] [![dep_status][s7]][l7] [![discord][s8]][l8]

</h1>

[s0]: https://github.com/gitui-org/gitui/workflows/CI/badge.svg
[l0]: https://github.com/gitui-org/gitui/actions
[s1]: https://img.shields.io/crates/v/gitui.svg
[l1]: https://crates.io/crates/gitui
[s2]: https://img.shields.io/badge/license-MIT-blue.svg
[s3]: https://img.shields.io/badge/unsafe-forbidden-success.svg
[l3]: https://github.com/rust-secure-code/safety-dance/
[s6]: https://img.shields.io/twitter/follow/extrawurst?label=follow&style=social
[l6]: https://twitter.com/intent/follow?screen_name=extrawurst
[s7]: https://deps.rs/repo/github/gitui-org/gitui/status.svg
[l7]: https://deps.rs/repo/github/gitui-org/gitui
[s8]: https://img.shields.io/discord/1176858176897953872
[l8]: https://discord.gg/rQNeEnMhus

<h5 align="center">GitUI provides you with the comfort of a git GUI but right in your terminal</h1>

> [!NOTE]
> **This is a personal fork** of [gitui](https://github.com/gitui-org/gitui) (maintained by [@jianshanbushishan](https://github.com/jianshanbushishan), regularly merged with upstream `master`).
> On top of upstream it adds `delta`-powered diffs, `bat`/`eza` file previews, inline image previews, in-file content search, large-repo performance work, and many UX refinements — see [Fork Enhancements](#fork-enhancements) for the full list.

![](demo.gif)

## <a name="table-of-contents"></a> Table of Contents

1. [Features](#features)
   - [Fork Enhancements over upstream](#fork-enhancements)
2. [Motivation](#motivation)
3. [Benchmarks](#bench)
4. [Roadmap](#roadmap)
5. [Limitations](#limitations)
6. [Installation](#installation)
7. [Build](#build)
8. [FAQs](#faqs)
9. [Diagnostics](#diagnostics)
10. [Color Theme](#theme)
11. [Key Bindings](#bindings)
12. [Sponsoring](#sponsoring)
13. [Inspiration](#inspiration)
14. [Contributing](#contributing)
15. [Contributors](#contributors)

## 1. <a name="features"></a> Features <small><sup>[Top ▲](#table-of-contents)</sup></small>

- Fast and intuitive **keyboard only** control
- Context based help (**no need to memorize** tons of hot-keys)
- Inspect, commit, and amend changes (incl. hooks: *pre-commit*,*commit-msg*,*post-commit*,*prepare-commit-msg*)
- Stage, unstage, revert and reset files, hunks and lines
- Stashing (save, pop, apply, drop, and inspect)
- Push / Fetch to / from remote
- Branch List (create, rename, delete, checkout, remotes)
- Browse / **Search** commit log, diff committed changes
- Responsive terminal UI
- Async git API for fluid control
- Submodule support
- gpg commit signing with shortcomings (see [#97](https://github.com/gitui-org/gitui/issues/97)))

### <a name="fork-enhancements"></a> Fork Enhancements <small><sup>[Top ▲](#table-of-contents)</sup></small>

Everything in this section is added or changed on top of upstream gitui (the fork is regularly merged with upstream `master`). Detailed per-release notes live in [CHANGELOG.md](./CHANGELOG.md).

#### Delta-powered diffs

- Diffs are rendered by [delta](https://github.com/dandavison/delta) when it is on `PATH` — **delta side-by-side is the default diff mode**, and `Alt+P` cycles between it and the built-in unified view (toggling into delta without the binary installed shows a hint to install it).
- In the unified diff view, `0` and `$` jump to the start/end of the (potentially long) diff line, and line numbers are shown.
- Delta rendering is async with an LRU cache and request dedupe; same-file stage/unstage re-renders synchronously, so the cursor never jumps and the view never flickers or shows stale content after staging hunks/lines.
- All gitui workflows work inside the delta preview: stage/unstage hunk & lines, reset hunk, per-file `(+added -deleted)` line counters in the diff title, and correct rendering of deleted files.
- Line wrapping, background highlighting and horizontal scrolling behave correctly in both delta modes, including CJK/double-width characters (wrapping uses unicode display width, not char count).
- delta (and bat) previews follow the **OS light/dark theme**, detected once at startup (on Windows via the `AppsUseLightTheme` registry key), so gitui's piped delta/bat output matches direct terminal runs.

#### File previews: bat, eza, images, in-content search

- File previews are syntax-highlighted by [bat](https://github.com/sharkdp/bat) when available (ANSI output rendered through gitui's own pipeline, `$BAT_THEME` respected), falling back to the built-in syntect highlighter — including bat line numbers in the file-history preview, and no plain-text flash before the highlighted result.
- **Images render inline** in preview panes via [ratatui-image](https://github.com/benjajaja/ratatui-image) terminal image protocols; newly added files are previewed at full content, including images. Image encoding runs on a background thread.
- Focusing a folder shows a directory listing (`eza`, falling back to `ls`) — as a depth-limited tree, configurable via `preview_tree_depth`.
- Press `f` while the preview pane is focused to **search within the file content** and jump between matches.
- bat's 256-color syntax output is expanded to exact RGB values so previews match direct bat output (notably on Windows), and external-tool discovery honors Windows `PATHEXT` (e.g. scoop's `bat.exe`).

#### Editor integration

- `e` opens the external editor **at the source line under the cursor** (`nvim +42 src/main.rs`) — in the status diff, inspect-commit, compare-commits and file-history views.
- `e` (edit file) and `c` (commit) also work while a diff pane is focused, not just from the file tree.
- Windows fixes: backslashes in configured editor paths are preserved, and the `VISUAL` environment variable is honored.

#### Status tab

- The Staged and Unstaged panes show live `(+N -M)` line-stat totals, and the Unstaged pane also shows the current branch with ahead/behind counts, on their top borders. The stats are computed off the UI thread via a single bulk diff.
- The commit popup accepts `Shift+Enter`/`Alt+Enter` as newline (in addition to the configured `newline` key) and **persists the message draft and cursor position** across sessions and restarts (per-repo).
- Pushing (branches and tags) and stash apply ask for confirmation first.

#### Revlog & commit details

- In **Logs**, press `b` to open the branch popup, then `v` (**View history**) to browse a selected local or remote branch without checking it out; `Enter` still checks out the branch. Remote history uses the remote-tracking refs already available locally.
- The log title shows `viewing` while browsing another branch. `Esc` clears an active search first, then returns to `HEAD` history. Navigation, commit details, diffs, copying and search remain available; reset, reword, revert, checkout, tag and push actions in the log are disabled in this mode.
- Commit details has a 3-way focus cycle `Info → Message → Files`; `y` copies the commit hash, the full commit message, or the whole author/date/sha/tags block, depending on which pane is focused.
- The Info pane is focusable with its own scrollbar and shows tags inline on one line; the author column keeps a stable fixed width; focused Files/Message panes show scrollbars when content overflows.
- Log search: `Ctrl+N`/`Ctrl+P` jump between matches with viewport centering, `j`/`k`/`g`/`G` move freely instead of being locked to matches, and the search status shows the cursor's position in the match range. Filename search is disabled by default for speed, and per-thread mailmap caching speeds up huge/NFS repositories.

#### Copy popup

- `Shift+Y` opens a copy popup that offers copying the file path or the **entire file content** (from the working tree in the Status tab, or from the selected revision in the files tree), with a labeled confirmation popup showing what was copied.

#### Performance (large repositories)

Tuned on giant working trees such as Unreal Engine projects:

- Status refreshes are coalesced and remote-progress notifications throttled, keeping the UI responsive during fetch/push.
- Line stats, log search and image encoding are moved off the UI thread; revlog and preview layout/rendering are cached.
- Fixed a stage/unstage hang and slow staging on large repos; commit search caches mailmap per thread.

#### Windows & CJK robustness

- Wide (CJK/emoji) graphemes no longer visually overflow popup borders, truncate wrapped delta lines, or break layout — a class of fixes across the diff view and popups.
- OS-theme detection for bat/delta, `PATHEXT`-aware tool discovery and true-color output as described above.

#### New configuration options

In the global `config.ron` (all optional):

```ron
(
    // left-pane width of the Status tab split, in percent
    status_left_ratio: Some(30),
    // left-pane width of the Revlog/Stash list split, in percent
    log_left_ratio: Some(30),
    // left-pane width of commit-details file/message split, in percent
    detail_left_ratio: Some(50),
    // recursion depth of the folder tree preview (default 2, clamped to [1, 10])
    preview_tree_depth: Some(3),
)
```

#### External diff tools

With a file diff focused, or a file selected in a list offering `Diff →`, press
`d` (`ExternalDiff`) to compare its complete old/new versions in
Beyond Compare (default), Neovim, or VS Code. In Options, select **External diff
tool**, press Enter to open the dropdown, use Up/Down, then Enter to save (Esc
cancels). The choice is saved per repository.

Merge the settings in [external-diff.ron](external-diff.ron) into your global
`config.ron` to customize executable paths and argument arrays. `{left}` and
`{right}` expand to temporary snapshots; keep the tool's wait behavior enabled.
Beyond Compare and VS Code run in the background while GitUI remains usable.
Neovim uses the current terminal and returns to GitUI when closed.
Snapshots are discarded when the viewer closes, so edits there do not modify
the worktree. The `external_diff` key binding is configurable; the default
binding for resetting selected diff lines is now `Alt+d` (Vim preset: `u`).

#### Miscellaneous

- The binary version string is derived from `git describe` and refreshes on every commit/checkout/tag.
- Dependency stack kept ahead of upstream: `git2` 0.21, `gix` 0.86, ratatui 0.30.

## 2. <a name="motivation"></a> Motivation <small><sup>[Top ▲](#table-of-contents)</sup></small>

I do most of my git work in a terminal but I frequently found myself using git GUIs for some use-cases like: index, commit, diff, stash, blame and log.

Unfortunately popular git GUIs all fail on giant repositories or become unresponsive and unusable.

GitUI provides you with the user experience and comfort of a git GUI but right in your terminal while being portable, fast, free and opensource.

## 3. <a name="bench"></a> Benchmarks <small><sup>[Top ▲](#table-of-contents)</sup></small>

For a [RustBerlin meetup presentation](https://youtu.be/rpilJV-eIVw?t=5334) ([slides](https://github.com/extrawurst/gitui-presentation)) I compared `lazygit`,`tig` and `gitui` by parsing the entire Linux git repository (which contains over 900k commits):

|           | Time       | Memory (GB) | Binary (MB) | Freezes   | Crashes   |
| --------- | ---------- | ----------- | ----------- | --------- | --------- |
| `gitui`   | **24 s** ✅ | **0.17** ✅  | 10         | **No** ✅  | **No** ✅  |
| `lazygit` | 57 s       | 2.6         | 25          | Yes       | Sometimes |
| `tig`     | 4 m 20 s   | 1.3         | **0.6** ✅   | Sometimes | **No** ✅  |

## 4. <a name="roadmap"></a> Road(map) to 1.0 <small><sup>[Top ▲](#table-of-contents)</sup></small>

These are the high level goals before calling out `1.0`:

* visualize branching structure in log tab ([#81](https://github.com/gitui-org/gitui/issues/81))
* interactive rebase ([#32](https://github.com/gitui-org/gitui/issues/32))
- no git-lfs support (see [#2812](https://github.com/gitui-org/gitui/issues/2812))

## 5. <a name="limitations"></a> Known Limitations <small><sup>[Top ▲](#table-of-contents)</sup></small>

- no sparse repo support (see [#1226](https://github.com/gitui-org/gitui/issues/1226))
- *credential.helper* for https needs to be **explicitly** configured (see [#800](https://github.com/gitui-org/gitui/issues/800))

Currently, this tool does not fully substitute the _git shell_, however both tools work well in tandem.

The priorities for `gitui` are on features that are making me mad when done on the _git shell_, like stashing, staging lines or hunks. Eventually, I will be able to work on making `gitui` a one stop solution - but for that I need help - this is just a spare time project for now.

All support is welcomed! Sponsors as well! ❤️

## 6. <a name="installation"></a> Installation <small><sup>[Top ▲](#table-of-contents)</sup></small>

GitUI is in beta and may contain bugs and missing features. However, for personal use it is reasonably stable and is being used while developing itself.

<a href="https://repology.org/project/gitui/versions">
    <img src="https://repology.org/badge/vertical-allrepos/gitui.svg" alt="Packaging status" align="right">
</a>

### Various Package Managers

<details>
  <summary>Install Instructions</summary>

##### [Arch Linux](https://archlinux.org/packages/extra/x86_64/gitui/)

```sh
pacman -S gitui
```

##### Fedora

```sh
sudo dnf install gitui
```

##### Gentoo
Available in [dm9pZCAq overlay](https://github.com/gentoo-mirror/dm9pZCAq)

```sh
sudo eselect repository enable dm9pZCAq
sudo emerge --sync dm9pZCAq
sudo emerge dev-vcs/gitui::dm9pZCAq
```

##### [openSUSE](https://software.opensuse.org/package/gitui)

```sh
sudo zypper install gitui
```

##### Homebrew (macOS)

```sh
brew install gitui
```

##### [MacPorts (macOS)](https://ports.macports.org/port/gitui/details/)

```sh
port install gitui
```

##### [Winget](https://github.com/microsoft/winget-pkgs/tree/master/manifests/s/StephanDilly/gitui) (Windows)

```
winget install gitui
```

##### [Scoop](https://github.com/ScoopInstaller/Main/blob/master/bucket/gitui.json) (Windows)

```
scoop install gitui
```

##### [Chocolatey](https://chocolatey.org/packages/gitui) (Windows)

```
choco install gitui
```

##### [Mise](https://github.com/jdx/mise)

```shell
mise use -g gitui@latest
```

##### [Nix](https://search.nixos.org/packages?channel=unstable&show=gitui&from=0&size=50&sort=relevance&query=gitui) (Nix/NixOS)

Nixpkg
```
nix-env -iA nixpkgs.gitui
```
NixOS
```
nix-env -iA nixos.gitui
```

##### [Termux](https://github.com/termux/termux-packages/tree/master/packages/gitui) (Android)

```
pkg install gitui
```

##### [Anaconda](https://anaconda.org/conda-forge/gitui)
```
conda install -c conda-forge gitui
```

</details>

### Release Binaries

[Available for download in releases](https://github.com/gitui-org/gitui/releases)

Binaries available for:

#### Linux

- gitui-linux-x86_64.tar.gz (linux musl statically linked)
- gitui-linux-aarch64.tar.gz (linux on 64 bit arm)
- gitui-linux-arm.tar.gz
- gitui-linux-armv7.tar.gz

All contain a single binary file

#### macOS

- gitui-mac.tar.gz (arm64)
- gitui-mac-x86.tar.gz (intel x86)

#### Windows

- gitui-win.tar.gz (single 64bit binary)
- gitui-win.msi (64bit Installer package)

### Nightly Builds

see [NIGHTLIES.md](./NIGHTLIES.md)

## 7. <a name="build"></a> Build <small><sup>[Top ▲](#table-of-contents)</sup></small>

### Requirements

- Minimum supported `rust`/`cargo` version: `1.88`
  - See [Install Rust](https://www.rust-lang.org/tools/install)

- To build openssl dependency (see https://docs.rs/openssl/latest/openssl/)
  - perl >= 5.12 (strawberry perl works for windows https://strawberryperl.com/)
  - a c compiler (msvc, gcc or clang, cargo will find it)

- To run the complete test suite python is required (and it must be invocable as `python`)

### Cargo Install

The simplest way to start playing around with `gitui` is to have `cargo` build and install it with `cargo install gitui --locked`. If you are not familiar with rust and cargo: [Getting Started with Rust](https://doc.rust-lang.org/book/ch01-00-getting-started.html)

### Cargo Features
#### trace-libgit
enable `libgit2` tracing

works if `libgit2` built with `-DENABLE_TRACE=ON`

this feature enabled by default, to disable: `cargo install --no-default-features`

## 8. <a name="faqs"></a> FAQs <small><sup>[Top ▲](#table-of-contents)</sup></small>

see [FAQs page](./FAQ.md)

## 9. <a name="diagnostics"></a> Diagnostics <small><sup>[Top ▲](#table-of-contents)</sup></small>

To run with logging enabled run `gitui -l`.

This will log to:

- macOS: `$HOME/Library/Caches/gitui/gitui.log`
- Linux using `XDG`: `$XDG_CACHE_HOME/gitui/gitui.log`
- Linux: `$HOME/.cache/gitui/gitui.log`
- Windows: `%LOCALAPPDATA%/gitui/gitui.log`

## 10. <a name="theme"></a> Color Theme <small><sup>[Top ▲](#table-of-contents)</sup></small>

![](assets/light-theme.png)

`gitui` should automatically work on both light and dark terminal themes.

However, you can customize everything to your liking: See [Themes](THEMES.md).

## 11. <a name="bindings"></a> Key Bindings <small><sup>[Top ▲](#table-of-contents)</sup></small>

The key bindings can be customized: See [Key Config](KEY_CONFIG.md) on how to set them to `vim`-like bindings.

## 12. <a name="sponsoring"></a> Sponsoring <small><sup>[Top ▲](#table-of-contents)</sup></small>

[![github](https://img.shields.io/badge/-GitHub%20Sponsors-fafbfc?logo=GitHub%20Sponsors)](https://github.com/sponsors/extrawurst)

## 13. <a name="inspiration"></a> Inspiration <small><sup>[Top ▲](#table-of-contents)</sup></small>

- [lazygit](https://github.com/jesseduffield/lazygit)
- [tig](https://github.com/jonas/tig)
- [GitUp](https://github.com/git-up/GitUp)
  - It would be nice to come up with a way to have the map view available in a terminal tool
- [git-brunch](https://github.com/andys8/git-brunch)

## 14. <a name="contributing"></a> Contributing <small><sup>[Top ▲](#table-of-contents)</sup></small>

See [CONTRIBUTING.md](CONTRIBUTING.md).

## 15. <a name="contributors"></a> Contributors <small><sup>[Top ▲](#table-of-contents)</sup></small>

Thanks goes to all the contributors that help make GitUI amazing! ❤️

Wanna become a co-maintainer? We are looking for [you](https://github.com/gitui-org/gitui/issues/2084)!

<a href="https://github.com/gitui-org/gitui/graphs/contributors">
  <img src="https://contrib.rocks/image?repo=gitui-org/gitui" />
</a>
