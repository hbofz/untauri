// A stand-in for a Tauri app: the same `(&str, &[u8])` asset table layout tauri-codegen
// emits (uncompressed, like an app built without the `compression` feature), printed at
// run time. tests/grow.rs builds it, grows a file with `untauri repack` and runs it, so
// the operating system's own loader proves the patched binary is sound.
static ASSETS: [(&str, &[u8]); 3] = [
    ("/index.html", b"<!doctype html><html><head><title>fixture</title></head><body><script src=/app.js></script></body></html>"),
    ("/app.js", b"console.log('fixture app');"),
    ("/style.css", b"body{margin:0}"),
];

fn main() {
    for (key, bytes) in std::hint::black_box(&ASSETS).iter() {
        println!("{key} {}", String::from_utf8_lossy(bytes));
    }
}
