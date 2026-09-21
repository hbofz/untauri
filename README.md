# untauri

Extracts the embedded web frontend (HTML/CSS/JS) out of a compiled Tauri app
binary. The `asar extract` Tauri never had.

## The problem

An Electron app ships its frontend as an `asar` archive, so `asar extract`
gets you the source straight back out. Tauri does the opposite: it
brotli-compresses the whole frontend and embeds it directly inside the Rust
binary at build time. There are no loose files, no archive, and no readable
manifest to point a tool at, so there was no equivalent way to get the
frontend back out of a shipped build. `untauri` locates and decompresses
those embedded assets directly from the binary.

## Install

```
cargo install --path .
```

or build it yourself:

```
cargo build --release
```

The binary is at `target/release/untauri`.

## Usage

```
untauri MyApp.app -o ./out
```

Also accepts a raw executable in place of a `.app` bundle:

```
untauri ./MyApp -o ./out
```

On Linux, point it at the ELF binary inside the package. Unpack the package
first (none of these run the app):

```
dpkg-deb -x MyApp.deb pkg          # .deb (or: ar x MyApp.deb && tar xf data.tar.*)
rpm2cpio MyApp.rpm | cpio -idm     # .rpm
7z x MyApp.AppImage -opkg          # .AppImage
untauri pkg/usr/bin/my-app -o ./out
```

Avoid `--appimage-extract` on an app you don't trust: it runs the AppImage.

Flags:

| Flag | Meaning |
|---|---|
| `-o, --out <DIR>` | Output directory (default `unpacked`) |
| `-v, --verbose` | Print the recovered asset table to stderr |
| `--raw` | Also dump streams that couldn't be classified as web assets |
| `--manifest-only` | Write only `assets.json`, don't extract files |
| `--tree` | Rebuild the bundle's original directory layout instead of grouping by kind |
| `--strict` | Exit non-zero if the recovered bundle references an asset that wasn't extracted |

## Output

A folder organized by kind, with each asset under its **original hashed
filename** wherever that can be recovered:

```
out/
  html/index.html
  css/main-[hash].css     css/vendor-[hash].css
  js/index-[hash].js     js/vendor-[hash].js    ...
  font/MyFont-[hash].woff2                       ...
  img/logo-[hash].png                            ...
  other/notify.mp3
  assets.json
```

`--tree` writes the same assets under their original key paths instead
(`index.html`, `assets/main-[hash].js`, `sw.js`), so the output directory can be
served as-is and the bundle's own relative imports resolve.

After writing, untauri diffs what the recovered bundle *references*
(`__vite__mapDeps` entries, `./chunk.js` import specifiers, root-absolute
`"/favicon.svg"` paths in HTML, CSS `url(...)`) against what it actually wrote, and warns
about anything missing. `--strict`
turns that warning into a non-zero exit. The check covers the local
JS/CSS/font/image names the bundle spells out; it does not chase references it
cannot parse from a static scan (dynamically-built paths, nested-directory
references, or extensions it does not recognise), so a clean result means
"nothing the scan could see is missing", not a proof of totality.

`assets.json` is a manifest listing every recovered stream: its written
name, the original name (when recovered), kind, byte offset in the source
binary, compressed length, and decompressed length.

## How it works

1. Parses the binary and locates its read-only data sections (`__const` and
   `__cstring` in a Mach-O, `.rodata` in an ELF).
2. Scans for the `/assets/` literal. Each asset is stored as
   `/assets/<filename><brotli stream>` with no length field, so every record
   announces itself and its own real filename. This is the primary
   enumeration.
3. Also runs an anchor-seeded walk that chains forward from the end of each
   decoded stream, and merges its results in de-duplicated by offset. The
   chain finds assets with no `/assets/` record of their own — the root
   `index.html` is the common case.
4. Decodes each candidate stream with a bounded brotli decoder.
5. Classifies each asset by its own extension when the name was recovered,
   falling back to content sniffing, and writes it out with a manifest.
6. Checks the recovered bundle's own references for anything missing.

Verified against a real Tauri app: recovered 479 assets
(`index.html`, both stylesheets, 402 JS chunks, 59 KaTeX fonts, 11 PNGs) in
one command, with every referenced asset resolved. The chain walk alone got
388 of them and said nothing about the other 91.

On Linux, verified against the x86_64 `.deb` builds of nine Tauri 1 and 2
apps: Kanri, Pot, Surrealist, Modrinth, Clash Verge Rev, Jan, Yaak,
Spacedrive and a Pake wrapper (Excalidraw), from 2 to 839 assets each, plus
Kanri's `.AppImage`.

## Limitations (read before relying on this)

- **Supports macOS Mach-O and Linux ELF binaries with brotli-compressed
  assets.** Windows binaries and any other compression scheme are not
  supported yet. Linux packages (`.deb`, `.rpm`, `.AppImage`) must be
  unpacked first; see Usage.
- **Content recovery is the guarantee. Original filenames are best-effort.**
  Almost every asset is written under its real hashed name (e.g.
  `main-[hash].css`), taken from the `/assets/` record that precedes its
  stream. Assets found only by chaining, with no record of their own, fall
  back to `<kind>/NNN.ext` (the keyless root document is named `index.html`).
  The bytes are always exact.
- **Linux builds lose more original filenames than macOS builds.** The ELF
  linker often pools an asset's key string with other strings instead of
  placing it right before the asset's stream, so the asset is found by the
  chain walk and comes out unnamed. Unnamed assets are typed by their
  content (`img/000.png`, `json/000.json`); one in a format untauri cannot
  recognise (e.g. pdf.js `.bcmap`) is only written with `--raw`. Comparing the
  same app versions on both platforms, that cost Yaak's Linux build 3 of 319
  assets, and nothing else was lost.
- **A single asset whose *compressed* size exceeds 4MB may not be
  recovered**, due to a bounded per-decode probe window. That is a real
  ceiling, not a soft one. It used to be 1MB, which sat under the largest
  asset in a real Tauri app (a ~2.3MB compressed image); that asset could not
  decode at all until the window was raised.
- **A stream that will not decode is not silently dropped.** If another
  asset shares its Vite content hash (Vite emits two names for identical
  content and stores the payload once), a copy is written under the failing
  record's name; otherwise the completeness check reports it.

## Intended use

Built for security research, auditing what a Tauri app actually ships in
its binary, and recovering your own frontend source from a build when you've
lost it. It is not positioned or intended for extracting other people's
products in order to clone them.

## License

MIT. See [LICENSE](LICENSE).
