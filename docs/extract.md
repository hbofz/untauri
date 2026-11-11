# Extracting

`untauri <app> -o <dir>` writes every file the app embeds to `<dir>`.

## Getting the binary

untauri reads the app's main executable, never runs it. Point it at a macOS `.app`
bundle (it finds the executable through `Info.plist`) or at the executable itself.
Packages and installers need unpacking first:

| Package | Unpack | Then point untauri at |
|---|---|---|
| `.deb` | `dpkg-deb -x MyApp.deb pkg` | `pkg/usr/bin/my-app` |
| `.rpm` | `rpm2cpio MyApp.rpm \| cpio -idm` | `usr/bin/my-app` |
| `.AppImage` | `7z x MyApp.AppImage -opkg` (`--appimage-extract` runs the app) | `pkg/usr/bin/my-app` |
| `.msi` | `7z x MyApp.msi -omsi` | the `.exe` inside `msi/` |
| `.app.tar.gz` | `tar xzf MyApp.app.tar.gz` | `MyApp.app` |
| `.apk` | `unzip MyApp.apk 'lib/*'` | `lib/arm64-v8a/libapp_lib.so` |

Universal macOS binaries work: untauri reads the first architecture, and every
architecture embeds the same frontend.

## Output

By default files are grouped by type (`js/`, `css/`, `font/`, `img/`, ...). With
`--tree` they keep the app's own paths (`index.html`, `assets/index-BzYEIace.js`), so
the output directory can be served directly or opened in an editor as the project
it came from.

Either way, `assets.json` lists every file: its output name, the app's own path for
it, its size, where it sits in the binary, and a hash that `repack` uses to make
sure the directory came from the same build.

| Flag | |
|---|---|
| `-o, --out <DIR>` | Output directory (default `unpacked`) |
| `--tree` | Keep the app's own folder layout |
| `--sources` | Rebuild the original source files from shipped source maps |
| `--strict` | Exit non-zero if any file failed to extract |
| `--manifest-only` | Write only `assets.json` |
| `--raw` | Also write streams that don't look like web files (fallback mode only) |
| `-v, --verbose` | Print every asset to stderr |

## Original source

Some apps ship source maps (`.js.map`), which often carry the complete original
source from before bundling and minification. `--sources` writes those files to
`<dir>/sources/` under their original paths, comments included. Most apps don't ship
source maps, and untauri says so when they don't. A map that lists a file without its
text can't rebuild it; those are counted in the summary.

## Missing references

After extracting, untauri scans the recovered HTML, JS and CSS for files they name
(dynamic imports, Vite's preload list, CSS `url()`) and reports any the app doesn't
contain. With Tauri's asset table these are never extraction gaps: the app loads them
from the network or a runtime path, or never uses them. `--strict` makes a real gap
(a file that failed to decode) exit non-zero.

## How it works

A Tauri app compiles its frontend into the executable as a table of
`(path, bytes)` pairs, where the bytes are brotli-compressed unless the app was built
without Tauri's `compression` feature. untauri finds that table by following its
pointers through the binary's relocations (ELF), chained fixups (Mach-O) or plain
addresses (PE), so it gets every file's real name and exact bytes rather than guessing.

When there's no table to read (32-bit builds, for example), it falls back to scanning
the binary for brotli streams. That finds the files but not always their names.

Each file is capped at 64 MB decompressed as a guard against decompression bombs. The
largest real asset seen so far is 11.4 MB. A file over the cap is reported, not
silently dropped.

## Color

Output is colored in a terminal. `NO_COLOR=1` turns it off, `CLICOLOR=0` and
`TERM=dumb` do too, and `CLICOLOR_FORCE=1` keeps color when you pipe the output.

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

Each one extracts in under a second.
