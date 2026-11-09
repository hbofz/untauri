use std::ops::Range;
use std::path::{Path, PathBuf};

pub struct Loaded { pub data: Vec<u8>, pub ranges: Vec<Range<usize>> }

pub fn load(path: &Path) -> std::io::Result<Loaded> {
    let bin = resolve_binary(path);
    let data = thin(std::fs::read(&bin)?);
    let ranges = ranges_for(&data);
    Ok(Loaded { data, ranges })
}

// A universal (fat) Mach-O holds one full binary per architecture, each with the same
// embedded frontend. Work on the first slice: its pointers are relative to the slice.
fn thin(data: Vec<u8>) -> Vec<u8> {
    use object::read::macho::{FatArch, MachOFatFile32, MachOFatFile64};
    let range = match object::FileKind::parse(&*data) {
        Ok(object::FileKind::MachOFat32) => MachOFatFile32::parse(&*data).ok().and_then(|f| f.arches().first().map(|a| a.file_range())),
        Ok(object::FileKind::MachOFat64) => MachOFatFile64::parse(&*data).ok().and_then(|f| f.arches().first().map(|a| a.file_range())),
        _ => None,
    };
    match range.and_then(|(o, s)| data.get(o as usize..(o + s) as usize)) {
        Some(slice) => slice.to_vec(),
        None => data,
    }
}

// The executable a .app bundle names in Info.plist (`CFBundleExecutable`). Contents/MacOS
// can hold helpers too (Jan ships `bun` and `uv` beside its app binary).
fn bundle_executable(app: &Path) -> Option<PathBuf> {
    let plist = std::fs::read_to_string(app.join("Contents/Info.plist")).ok()?;
    let after = &plist[plist.find("<key>CFBundleExecutable</key>")?..];
    let start = after.find("<string>")? + "<string>".len();
    let name = after[start..].split("</string>").next()?.trim();
    let p = app.join("Contents/MacOS").join(name);
    p.is_file().then_some(p)
}

// Resolve a .app bundle to its main Mach-O, else the path itself.
fn resolve_binary(path: &Path) -> PathBuf {
    if path.extension().and_then(|e| e.to_str()) == Some("app") {
        if let Some(p) = bundle_executable(path) { return p; }
        let macos = path.join("Contents/MacOS");
        if let Ok(rd) = std::fs::read_dir(&macos) {
            if let Some(first) = rd.flatten().find(|e| e.path().is_file()) {
                return first.path();
            }
        }
    }
    path.to_path_buf()
}

fn ranges_for(data: &[u8]) -> Vec<Range<usize>> {
    use object::{Object, ObjectSection};
    let mut ranges = Vec::new();
    if let Ok(file) = object::File::parse(data) {
        for sec in file.sections() {
            if let Ok(name) = sec.name() {
                // Mach-O keeps the embedded assets in __const/__cstring, ELF in .rodata.
                if matches!(name, "__const" | "__cstring" | ".rodata") {
                    if let Some((off, size)) = sec.file_range() {
                        let (o, s) = (off as usize, size as usize);
                        if let Some(e) = o.checked_add(s) {
                            if e <= data.len() && s > 0 { ranges.push(o..e); }
                        }
                    }
                }
            }
        }
    }
    if ranges.is_empty() { ranges.push(0..data.len()); }
    ranges
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn falls_back_to_whole_file_for_non_macho() {
        let data = vec![1u8, 2, 3, 4, 5, 6, 7, 8];
        assert_eq!(ranges_for(&data), vec![0..8]);
    }
    #[test]
    fn scans_only_elf_rodata() {
        use object::write::{Object as W, StreamingBuffer};
        use object::{Architecture, BinaryFormat, Endianness, SectionKind};
        let mut w = W::new(BinaryFormat::Elf, Architecture::X86_64, Endianness::Little);
        let text = w.add_section(vec![], b".text".to_vec(), SectionKind::Text);
        w.append_section_data(text, &[0x90; 16], 1);
        let ro = w.add_section(vec![], b".rodata".to_vec(), SectionKind::ReadOnlyData);
        w.append_section_data(ro, b"/assets/x.js", 1);
        let mut data = Vec::new();
        w.emit(&mut StreamingBuffer::new(&mut data)).unwrap();
        let r = ranges_for(&data);
        assert_eq!(r.len(), 1);
        assert_eq!(&data[r[0].clone()], b"/assets/x.js");
    }
    #[test]
    fn resolve_returns_path_itself_for_plain_file() {
        let p = Path::new("/tmp/whatever-binary");
        assert_eq!(resolve_binary(p), p.to_path_buf());
    }
}
