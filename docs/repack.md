# Repacking

`untauri repack <app> <dir> -o <out>` writes the files you edited in `<dir>` back into
the app and saves the result as `<out>`. The original is never modified.

```
untauri MyApp.app -o out --tree
# edit files in out/
untauri repack MyApp.app out -o patched
```

Use `--tree` when extracting: then `out/index.html` is the app's real root page. The
default layout works too, but a multi-page app has several `index.html` files there,
and `assets.json` is the only way to tell which is which.

## What gets written

Only files that differ from the app's own are written back. Each is compressed (if
the app's files are) at whichever brotli level gives the smallest result, which is
often smaller than Tauri's own output, so edits usually have some room to grow in
place. A file that still fits is written over the original bytes and nothing else in
the binary changes. A file that doesn't fit goes into new space at the end of the
binary, and its table entry is pointed there.

## Checks

Before writing anything, repack:

- reads the patched binary back the same way extraction does, and checks that every
  edited file comes out as you left it and every other file is unchanged;
- refuses a directory extracted from a different build of the app (each file's hash
  is recorded in `assets.json`), so edits made against one version never land in
  another;
- refuses edits to files the linker stored as one copy (identical files, like Nuxt's
  `index.html` and `404.html`) unless every copy gets the same edit.

If any check fails, nothing is written. `--dry-run` runs all of it and stops before
writing.

## Growing files, per platform

Linux and Android (ELF): the binary's `PT_NOTE` program header, build metadata the
loader doesn't use, becomes a new loadable segment for the grown files; the note data
itself stays in the file. The segment gets its own `.untauri` section, so stripping
the binary afterwards keeps it. A binary without section headers, or with Android's
packed relocations, can't be grown.

Windows (PE): the grown files go in a new `.untauri` section. A signature at the end
of the file is removed (it no longer matches anyway) and the PE checksum, when the
file has one, is recomputed. An executable with other data after its last section
(some installers append a payload) or with no room left for another section header
is refused.

macOS (Mach-O): the grown files go in a new `__UNTAURI` segment just before
`__LINKEDIT`, which moves up to make room, since codesign needs it last. The
segment's load command uses the padding the linker leaves after the load commands; a
binary without room is refused. Apple Silicon won't run the patched binary until it
is signed again (see below). Universal apps, which hold one binary per architecture,
can't grow yet: in those, an edited file has to fit in the space its original took.

Repacking an already repacked binary reuses that same space.

## After repacking

Patching breaks code signatures. On macOS, put the binary back in its `.app` and
re-sign it before launching:

```
codesign --force --deep --preserve-metadata=entitlements -s - MyApp.app
```

On Windows, a signed app loses its signature, so Windows may warn before running it.

If the app sets a Content Security Policy, Tauri allows its inline `<script>` blocks
by their build-time hash, so an edited inline script is blocked. Edits to `.js` files
are not affected.
