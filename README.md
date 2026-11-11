# untauri

[![crates.io](https://img.shields.io/crates/v/untauri.svg)](https://crates.io/crates/untauri)
[![license](https://img.shields.io/crates/l/untauri.svg)](LICENSE)

Extract, inspect and patch the web frontend inside compiled Tauri apps.

![untauri listing, extracting, inspecting and repacking Kanri](docs/demo.gif)

## Install

Prebuilt binaries for Linux, macOS and Windows are on the
[releases page](https://github.com/hbofz/untauri/releases/latest). With Rust:

```
cargo install untauri
```

## Quick start

```
untauri MyApp.app -o out                 # extract every file
untauri list MyApp.app                   # see what's inside, with sizes
untauri inspect MyApp.app                # Tauri version, crates, plugins, IPC calls
untauri repack MyApp.app out -o patched  # write your edits back
```

The app can be a macOS `.app` bundle or an executable: Linux ELF, Windows `.exe`,
macOS Mach-O or an Android `.so`. Unpack installers first, without running them:

```
dpkg-deb -x MyApp.deb pkg        # .deb
7z x MyApp.msi -omsi             # .msi
7z x MyApp.AppImage -opkg        # .AppImage (--appimage-extract would run it)
```

Other formats, like `.rpm` and `.apk`: [docs/extract.md](docs/extract.md#getting-the-binary).

## Extract

```
$ untauri kanri -o out --tree
read Tauri's asset table: 61 entries (brotli)
recovered 61 assets -> out
complete: all 61 entries in the app's asset table were extracted.
note: 1 name(s) referenced by the bundle are not in the app's asset table (loaded from elsewhere at runtime, or unused):
  not embedded: kanri_theme_export.json
```

`--tree` keeps the app's own folder layout, so `out/` can be served as-is. Without
it, files are grouped by type. If the app ships source maps, `--sources` rebuilds
the original source too:

```
$ untauri yaak -o out --sources
...
sources: rebuilt 1691 original files from 61 source maps -> out/sources (1221 from node_modules)
```

All flags: `untauri --help`. More in [docs/extract.md](docs/extract.md).

## List

```
$ untauri list kanri
61 assets (560.2 KiB brotli, 2.7 MiB decompressed)
├── 200.html (3.2 KiB, 983 B stored)
├── 404.html (3.2 KiB, 983 B stored)
├── _nuxt
│   ├── 6S4jwJWB.js (110.0 KiB, 9.3 KiB stored)
...
```

`--json` prints the same as JSON.

## Inspect

```
$ untauri inspect kanri
binary    Elf X86_64
tauri     2.8.5
runtime   tao 0.34.3, tauri-runtime 2.8.0, tauri-runtime-wry 2.8.1, tauri-utils 2.7.0, wry 0.53.3
plugins   autostart 2.5.0, dialog 2.4.0, fs 2.4.2, log 2.7.0, os 2.3.1, persisted-scope 2.3.2, ...
crates    90 found with versions, a lower bound (--lockfile to audit them)
frontend  61 files, 2732 KB, brotli

IPC referenced by the frontend: 30 plugin commands, 0 literal invoke() calls.
Both are lower bounds, and referenced is not granted: the app's capabilities decide what runs.
  dialog: ask, message, open, save
  fs: exists, read_text_file, write_text_file
  ...
```

To check the crates it found for known vulnerabilities:

```
untauri inspect MyApp --lockfile app.lock
cargo audit -f app.lock
```

Every list here is a lower bound, so a crate or call that's missing may still be in
the app. Why, and what a clean audit does and doesn't tell you:
[docs/inspect.md](docs/inspect.md).

## Repack

```
$ untauri kanri -o out --tree
$ sed -i 's|<head>|<head><title>Patched</title>|' out/index.html
$ untauri repack kanri out -o kanri-patched
patched 1 asset(s) -> kanri-patched
  /index.html
```

- Edited files can be any size. One that no longer fits where the original was is
  moved to new space at the end of the binary. (Not yet in universal macOS apps,
  where an edit has to fit.)
- `--dry-run` does everything except write the file.
- Patching breaks code signatures. On macOS, re-sign before launching:
  `codesign --force --deep --preserve-metadata=entitlements -s - MyApp.app`
- The linker stores identical files once (Nuxt's `index.html` and `404.html`, for
  example). Give them the same edit, or none.
- If the app sets a Content Security Policy, an edited inline `<script>` is blocked.
  Edit the `.js` files instead.

How it works, and the few cases it refuses: [docs/repack.md](docs/repack.md).

## Limits

- You get the built frontend, bundled and minified. The Rust backend isn't
  recovered.
- 32-bit apps fall back to scanning the binary, which can miss file names.
- A file over 64 MB decompressed is reported, not extracted.

Verified byte for byte on ten real apps, from Modrinth (877 files) to Pake (1):
[list](docs/extract.md#tested-on).

## Responsible use

Use untauri only on software you own or have explicit permission to analyze. It
gives you no right to copy, modify or redistribute someone else's app, and you are
responsible for complying with the license and terms of any software you use it on.

## License

MIT. Provided as is, without warranty; see [LICENSE](LICENSE).
