# untauri

[![crates.io](https://img.shields.io/crates/v/untauri.svg)](https://crates.io/crates/untauri)
[![license](https://img.shields.io/crates/l/untauri.svg)](LICENSE)

Extract, inspect and patch the web frontend inside compiled Tauri apps.

![untauri listing, extracting, inspecting and repacking Kanri](docs/demo.gif)

A Tauri app doesn't ship its frontend as loose files. Every HTML, JS, CSS, font and
image file is compressed and compiled into the executable, indexed by a table of
paths. untauri reads that table directly, so every file comes back under its real
name, byte for byte. It works on Linux, Windows and macOS binaries (universal builds
included) and on Android libraries.

## Install

Prebuilt binaries for Linux, macOS and Windows are on the
[releases page](https://github.com/hbofz/untauri/releases/latest). With Rust:

```
cargo install untauri
```

## Usage

| Command | |
|---|---|
| `untauri <app> -o out` | Extract every file |
| `untauri list <app>` | List what the app embeds, with sizes |
| `untauri inspect <app>` | Report the Tauri version, crates, plugins and IPC calls |
| `untauri repack <app> out -o patched` | Write edited files back into the app |

`<app>` is a macOS `.app` bundle or an executable: ELF, PE (`.exe`), Mach-O, or an
Android `.so`. Installers and packages need unpacking first. None of these commands
run the app:

```
dpkg-deb -x MyApp.deb pkg          # .deb
rpm2cpio MyApp.rpm | cpio -idm     # .rpm
7z x MyApp.AppImage -opkg          # .AppImage (--appimage-extract would run it)
7z x MyApp.msi -omsi               # .msi
untauri pkg/usr/bin/my-app -o out
```

## Extract

```
untauri MyApp.app -o out
```

Files are grouped by type (`js/`, `css/`, `font/`, ...). With `--tree` they keep the
app's own layout (`index.html`, `assets/index-BzYEIace.js`), so the output can be
served as-is. Either way, `out/assets.json` records each file's path, size and offset
in the binary.

After extracting, untauri checks the recovered JS, CSS and HTML for files they
reference and reports any the app doesn't contain. Those are usually loaded from the
network, or never used.

| Flag | |
|---|---|
| `-o, --out <DIR>` | Output directory (default `unpacked`) |
| `--tree` | Keep the app's own folder layout |
| `--sources` | Rebuild the original source files from shipped source maps |
| `--strict` | Exit non-zero if any file failed to extract |
| `--manifest-only` | Write only `assets.json` |
| `--raw` | Also write streams that don't look like web files (fallback mode only) |
| `-v, --verbose` | Print every asset to stderr |

### Original source

Some apps ship source maps, which often carry the complete original source from
before bundling and minification. `--sources` writes it to `out/sources/`:

```
$ untauri yaak -o out --sources
...
sources: rebuilt 1691 original files from 61 source maps -> out/sources (1221 from node_modules)
```

That is Yaak's own TypeScript (`apps/yaak-client/commands/importCurl.ts`, ...) as it
was written, comments included. Most apps don't ship source maps, and untauri says
so when they don't.

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

`--json` prints each file's path, offset, stored size and decompressed size.

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

Crate versions come from the source paths the Rust compiler leaves in release builds.
Each version is exact, but only crates with code that can panic leave a path, so the
list is a lower bound: macro and build-time crates never appear. Dependencies built
from git are listed with their commit. To check the crates found for known
vulnerabilities:

```
untauri inspect MyApp --lockfile app.lock
cargo audit -f app.lock
```

A clean audit means none of the crates found have advisories, not that the app has
none. The IPC list is a lower bound too: it catches `plugin:` strings and literal
`invoke("...")` calls, but not calls made through a minified alias. `--json` prints
the full report.

## Repack

```
untauri MyApp.app -o out --tree
# edit out/index.html, out/assets/main-A1b2C3d4.js, ...
untauri repack MyApp.app out -o patched
```

Only files that differ from the app's originals are written back. Each is compressed
at whichever brotli level gives the smallest result and written over the original
bytes, and its table entry's length is updated. Nothing else in the binary moves.
`--dry-run` does everything except write the file.

Before writing, untauri reads the patched binary back and checks that every edited
file comes out as you left it and every other file is unchanged. It refuses a
directory extracted from a different build of the app, so edits made against one
version never land in another.

- An edited file has to fit in the space its original took. Large files
  usually have a few KB to spare and tiny ones almost none. In uncompressed builds a
  file can't grow at all.
- The linker stores identical files once (Nuxt's `index.html` and
  `404.html`, for example). Repack names them when it finds them; make the same edit
  to all of them, or to none.
- Patching breaks the code signature. On macOS, put the binary back
  in its `.app` and re-sign it with
  `codesign --force --deep --preserve-metadata=entitlements -s - MyApp.app`. On
  Windows, a signed app loses its valid signature.
- If the app sets a Content Security Policy, Tauri allows its inline
  `<script>` blocks by their build-time hash, so an edited inline script is blocked.
  Edits to `.js` files are not affected.

## Tested on

Every file, under its real name, compared byte for byte against the app's own table:

| App | Platform | Files |
|---|---|---|
| Modrinth | Linux | 877 |
| Surrealist | Linux, macOS arm64 | 648 |
| Jan | Linux, macOS universal | 559 |
| Yaak | Linux | 321 |
| Clash Verge Rev | Linux | 275 |
| Spacedrive | Linux | 200 |
| Pot | Linux | 180 |
| End-WikiPlus | Android arm64 | 76 |
| Kanri | Linux, macOS arm64 + x64, Windows | 61 |
| Pake (ChatGPT) | Linux | 1 |

Each takes under a second.

## Limits

- You get the built frontend: bundled and minified, and without the Rust backend.
- Each file is capped at 64 MB decompressed, as a guard against decompression bombs.
  The largest real asset seen so far is 11.4 MB. A file over the cap is reported, not
  silently dropped.
- The table reader handles 64-bit binaries. 32-bit builds (i686 Windows, for example)
  fall back to scanning the binary for compressed streams, which recovers the files
  but not always their names.

## Responsible use

Use untauri only on software you own or have explicit permission to analyze. It
gives you no right to copy, modify or redistribute someone else's app, and you are
responsible for complying with the license and terms of any software you use it on.

## License

MIT. Provided as is, without warranty; see [LICENSE](LICENSE).
