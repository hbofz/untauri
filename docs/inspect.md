# Inspecting

`untauri inspect <app>` reports what an app is built with and what its frontend
calls, without extracting anything. `--json` prints the full report, including every
crate.

## Crate versions

Release builds of Rust programs keep the source paths of code that can panic, and
those paths name the crate and its exact version
(`.../registry/src/.../tauri-2.8.5/src/...`). untauri collects them. Each version is
exact, but a crate only shows up if some of its code can panic, so the list is a
lower bound: macro and build-time crates never appear. Dependencies built from git,
like a Tauri commit, are listed with that commit instead of a version.

## Auditing

`--lockfile <file>` writes every crate found as a `Cargo.lock`, which `cargo audit`
can check against the RustSec advisory database:

```
untauri inspect MyApp --lockfile app.lock
cargo audit -f app.lock
```

A clean audit means none of the crates found have advisories. It doesn't mean the
app has none: a crate that leaves no path can't be checked.

## IPC

The IPC section lists the Tauri plugin commands (`plugin:fs|read_text_file`) and the
literal `invoke("...")` calls the frontend's code references. It is a lower bound
too: a call made through a minified alias isn't caught. And referenced is not
granted: the app's capabilities decide which of these commands actually run.
