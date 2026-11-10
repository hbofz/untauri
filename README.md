# untauri

Get the web frontend (HTML, CSS, JS, fonts, images) back out of a compiled Tauri app.

![untauri extracting Jan's frontend, then inspecting Yaak](docs/demo.gif)

Tauri doesn't ship your frontend as loose files. It compresses every file and bakes
it into the executable, with a lookup table of names. untauri reads that table and
writes every file back out under its original name.

> [!WARNING]
> Only use untauri on software you own or have explicit permission to analyze.
> It does not give you any right to copy, modify or redistribute someone else's app,
> and you are responsible for complying with the license and terms of any software
> you run it on. Provided as is, without warranty (see [LICENSE](LICENSE)).

## Install

Download a prebuilt binary for Windows, macOS or Linux from
[Releases](https://github.com/hbofz/untauri/releases/latest), or with Rust installed:

```
cargo install untauri
```

## Usage

```
untauri MyApp.app -o out          # macOS bundle
untauri ./my-app -o out           # any executable: Linux ELF, Windows .exe, Mach-O
```

Linux packages need unpacking first. None of these run the app:

```
dpkg-deb -x MyApp.deb pkg          # .deb
rpm2cpio MyApp.rpm | cpio -idm     # .rpm
7z x MyApp.AppImage -opkg          # .AppImage (don't use --appimage-extract, it runs the app)
untauri pkg/usr/bin/my-app -o out
```

For Windows, `7z x MyApp.msi -omsi` gets you the `.exe`.

| Flag | |
|---|---|
| `-o, --out <DIR>` | Output directory (default `unpacked`) |
| `--tree` | Keep the app's own folder layout, so you can serve the output directly |
| `--sources` | Also rebuild the original source files from shipped source maps |
| `--strict` | Exit non-zero if any file failed to extract |
| `--manifest-only` | Only write `assets.json` |
| `--raw` | Also write streams that don't look like web files (fallback mode only) |
| `-v, --verbose` | Print every asset to stderr |

By default files are grouped by type (`js/`, `css/`, `font/`, ...). `--tree` writes them
where the app expects them (`index.html`, `assets/index-BzYEIace.js`). Either way you
get `assets.json`, listing each file's name, size and offset in the binary.

After extracting, untauri also scans the recovered JS, CSS and HTML for files they
reference and tells you about any that aren't in the app. Those are usually loaded from
the network or never used.

## Original source

Some apps ship their source maps (`.js.map`). Those often contain the full original
source, before bundling and minifying. `--sources` writes it out to `out/sources/`:

```
$ untauri yaak -o out --sources
...
sources: rebuilt 1691 original files from 61 source maps -> out/sources (1221 from node_modules)
```

That is Yaak's own TypeScript (`apps/yaak-client/commands/importCurl.ts`, ...) as it
was written, comments included. Most apps don't ship source maps, and untauri says so.

## Inspect

`untauri inspect` reports what an app is built with and what its frontend calls,
without extracting anything:

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

Versions come from the source paths the Rust compiler leaves in release builds. Each
one is exact, but only crates with code that can panic leave a path, so the list is a
lower bound (macro and build-time crates never appear). Dependencies built from git,
like a Tauri commit, are listed with their commit instead. To check the crates found
for known vulnerabilities:

```
untauri inspect MyApp --lockfile app.lock
cargo audit -f app.lock
```

A clean audit does not prove the app is clean, only that none of the crates found
have advisories. The IPC list is also a lower bound: it catches `plugin:` strings and
literal `invoke("...")` calls, but not calls made through a minified alias.

`--json` prints the full report, including every crate.

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
| Kanri | Linux, macOS arm64 + x64, Windows | 61 |
| Pake (ChatGPT) | Linux | 1 |

Each one takes under a second.

## Limits

- Each file is capped at 64MB decompressed, as a guard against decompression bombs.
  The largest real asset seen so far is 11.4MB. A file over the cap is reported, not
  silently dropped.
- The table reader handles 64-bit binaries. 32-bit builds (i686 Windows, for example)
  fall back to the scanner.
- You get the built frontend, not your source: minified, bundled, without the Rust side.

## License

MIT
