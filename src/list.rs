// `untauri list`: what an app embeds, as a tree of its asset paths with sizes, without
// writing anything.

use crate::target::Loaded;
use serde::Serialize;
use std::collections::BTreeMap;
use std::process::ExitCode;

#[derive(Serialize)]
struct Item {
    path: String,
    // Where the stored bytes start in the binary. Identical files the linker stored once
    // share an offset, so sum `stored_len` over distinct offsets for the true total.
    offset: usize,
    // Bytes as stored in the binary (brotli, or raw for an uncompressed build).
    stored_len: usize,
    // None when the stored bytes do not decode.
    decompressed_len: Option<usize>,
}

#[derive(Default)]
struct Node { children: BTreeMap<String, Node>, item: Option<(usize, Option<usize>)> }

fn human(n: usize) -> String {
    match n {
        n if n < 1024 => format!("{n} B"),
        n if n < 1 << 20 => format!("{:.1} KiB", n as f64 / 1024.0),
        n => format!("{:.1} MiB", n as f64 / (1 << 20) as f64),
    }
}

fn render(node: &Node, prefix: &str, out: &mut String) {
    let last = node.children.len().saturating_sub(1);
    for (i, (name, child)) in node.children.iter().enumerate() {
        let (branch, indent) = if i == last { ("└── ", "    ") } else { ("├── ", "│   ") };
        let size = match child.item {
            Some((s, Some(d))) if s == d => format!(" ({})", human(d)),
            Some((s, Some(d))) => format!(" ({}, {} stored)", human(d), human(s)),
            Some((s, None)) => format!(" ({} stored, does not decode)", human(s)),
            None => String::new(),
        };
        out.push_str(&format!("{prefix}{branch}{name}{size}\n"));
        render(child, &format!("{prefix}{indent}"), out);
    }
}

fn tree(items: &[Item]) -> String {
    let mut root = Node::default();
    for it in items {
        let mut node = &mut root;
        for part in it.path.split('/').filter(|p| !p.is_empty()) {
            node = node.children.entry(part.to_string()).or_default();
        }
        node.item = Some((it.stored_len, it.decompressed_len));
    }
    let mut out = String::new();
    render(&root, "", &mut out);
    out
}

pub fn run(loaded: &Loaded, json: bool) -> ExitCode {
    let Some(t) = crate::table::read(&loaded.data) else {
        say!("no Tauri asset table found, so there are no asset paths to list. `untauri <app>` can still scan for the files.");
        return ExitCode::from(2);
    };
    let mut items: Vec<Item> = t.assets.iter()
        .map(|a| Item { path: a.path.clone().unwrap_or_default(), offset: a.offset, stored_len: a.compressed_len, decompressed_len: Some(a.bytes.len()) })
        .chain(t.undecoded.iter().map(|k| {
            let (offset, stored_len) = t.spans.get(k).copied().unwrap_or_default();
            Item { path: k.clone(), offset, stored_len, decompressed_len: None }
        }))
        .collect();
    items.sort_by(|a, b| a.path.cmp(&b.path));
    let text = if json { serde_json::to_string_pretty(&items).unwrap() + "\n" } else {
        // Identical files the linker stored once count once toward the binary's bytes.
        let spans: std::collections::HashMap<usize, usize> = items.iter().map(|i| (i.offset, i.stored_len)).collect();
        let stored: usize = spans.values().sum();
        let full: usize = items.iter().filter_map(|i| i.decompressed_len).sum();
        let how = if t.compressed { format!("{} brotli, {} decompressed", human(stored), human(full)) } else { format!("{} stored, uncompressed", human(stored)) };
        format!("{} assets ({how})\n{}", items.len(), tree(&items))
    };
    // Ignore a closed pipe (`untauri list app | head`) instead of panicking.
    use std::io::Write;
    let _ = std::io::stdout().write_all(text.as_bytes());
    ExitCode::SUCCESS
}

#[cfg(test)]
mod tests {
    use super::*;

    // Mutation check: swap the last-child test and the `└──`/`│` connectors come out
    // wrong; drop the path split and nested files print flat.
    #[test]
    fn renders_nested_paths_as_a_tree() {
        let it = |p: &str, s, d| Item { path: p.into(), offset: 0, stored_len: s, decompressed_len: d };
        let out = tree(&[it("/assets/a.js", 10, Some(40)), it("/assets/b.css", 5, None), it("/index.html", 7, Some(7))]);
        assert_eq!(out, "├── assets\n│   ├── a.js (40 B, 10 B stored)\n│   └── b.css (5 B stored, does not decode)\n└── index.html (7 B)\n");
    }
}
