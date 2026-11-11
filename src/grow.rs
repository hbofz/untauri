// Room for files that outgrow their original span: append them in a new loadable segment
// at the end of the binary and point their table entries there.
//
// ELF (Linux, Android): the program header table has no spare entry, so the PT_NOTE entry
// becomes the new PT_LOAD. Notes are build metadata (build id, ABI tag) that the loader
// does not need; the note sections themselves stay in the file. The entry moves to just
// after the last PT_LOAD, because loaders expect PT_LOADs sorted by address. A later
// repack of the same binary extends that segment instead of needing another note.
// ponytail: ELF only so far; PE and Mach-O each need their own segment surgery.

const PT_LOAD: u32 = 1;
const PT_DYNAMIC: u32 = 2;
const PT_NOTE: u32 = 4;
const PT_GNU_PROPERTY: u32 = 0x6474_e553;
const DT_RELA: u64 = 7;
const DT_RELASZ: u64 = 8;
// R_X86_64_RELATIVE, R_AARCH64_RELATIVE, R_RISCV_RELATIVE
const RELATIVE: [u32; 3] = [8, 1027, 3];

#[derive(Clone, Copy)]
struct Phdr { kind: u32, flags: u32, offset: u64, vaddr: u64, filesz: u64, memsz: u64, align: u64 }

fn u16_at(d: &[u8], o: usize) -> u16 { u16::from_le_bytes(d[o..o + 2].try_into().unwrap()) }
fn u32_at(d: &[u8], o: usize) -> u32 { u32::from_le_bytes(d[o..o + 4].try_into().unwrap()) }
fn u64_at(d: &[u8], o: usize) -> u64 { u64::from_le_bytes(d[o..o + 8].try_into().unwrap()) }
fn put_u64(d: &mut [u8], o: usize, v: u64) { d[o..o + 8].copy_from_slice(&v.to_le_bytes()); }
fn round_up(v: u64, a: u64) -> u64 { v.div_ceil(a) * a }

pub fn supported(data: &[u8]) -> bool {
    data.len() > 64 && data.starts_with(b"\x7fELF") && data[4] == 2 && data[5] == 1 && u16_at(data, 0x36) == 56
}

// (offset of the table in the file, entries)
fn phdrs(data: &[u8]) -> Result<(usize, Vec<Phdr>), String> {
    let (off, n) = (u64_at(data, 0x20) as usize, u16_at(data, 0x38) as usize);
    n.checked_mul(56).and_then(|s| s.checked_add(off)).filter(|&e| e <= data.len()).ok_or("program header table is out of bounds")?;
    Ok((off, (0..n).map(|i| {
        let o = off + i * 56;
        Phdr { kind: u32_at(data, o), flags: u32_at(data, o + 4), offset: u64_at(data, o + 8), vaddr: u64_at(data, o + 16),
               filesz: u64_at(data, o + 32), memsz: u64_at(data, o + 40), align: u64_at(data, o + 48) }
    }).collect()))
}

fn write_phdrs(data: &mut [u8], off: usize, ph: &[Phdr]) {
    for (i, p) in ph.iter().enumerate() {
        let o = off + i * 56;
        data[o..o + 4].copy_from_slice(&p.kind.to_le_bytes());
        data[o + 4..o + 8].copy_from_slice(&p.flags.to_le_bytes());
        for (f, v) in [(8, p.offset), (16, p.vaddr), (24, p.vaddr), (32, p.filesz), (40, p.memsz), (48, p.align)] { put_u64(data, o + f, v); }
    }
}

fn file_to_vaddr(ph: &[Phdr], off: u64) -> Option<u64> {
    ph.iter().find(|p| p.kind == PT_LOAD && off >= p.offset && off - p.offset < p.filesz).map(|p| off - p.offset + p.vaddr)
}
fn vaddr_to_file(ph: &[Phdr], va: u64) -> Option<usize> {
    ph.iter().find(|p| p.kind == PT_LOAD && va >= p.vaddr && va - p.vaddr < p.filesz).map(|p| (va - p.vaddr + p.offset) as usize)
}

// Append `blobs` to the binary, mapped read-only. Returns each blob's virtual address.
pub fn append(data: &mut Vec<u8>, blobs: &[Vec<u8>]) -> Result<Vec<u64>, String> {
    if !supported(data) { return Err("growing a file past its original size is supported for 64-bit ELF (Linux, Android) so far".into()); }
    let (off, mut ph) = phdrs(data)?;
    let loads: Vec<usize> = (0..ph.len()).filter(|&i| ph[i].kind == PT_LOAD).collect();
    let top = loads.iter().map(|&i| ph[i].vaddr + ph[i].memsz).max().ok_or("no loadable segments")?;
    let align = loads.iter().map(|&i| ph[i].align).max().unwrap_or(0).max(0x1000);
    let file_end = data.len() as u64;

    // Our own segment from an earlier repack: the highest one, ending at the end of the
    // file, with no zero-filled tail. Extend it.
    let ours = loads.iter().copied().find(|&i| {
        let p = ph[i];
        p.vaddr + p.memsz == top && p.offset + p.filesz == file_end && p.filesz == p.memsz
    });
    let (seg, start) = match ours {
        Some(i) => (i, file_end),
        None => {
            let props: Vec<(u64, u64)> = ph.iter().filter(|p| p.kind == PT_GNU_PROPERTY).map(|p| (p.offset, p.filesz)).collect();
            let note = (0..ph.len()).rev().find(|&i| ph[i].kind == PT_NOTE && !props.contains(&(ph[i].offset, ph[i].filesz)))
                .ok_or("the binary has no spare program header (PT_NOTE) to map new data with")?;
            let start = round_up(file_end, align);
            ph.remove(note);
            let p = Phdr { kind: PT_LOAD, flags: 4, offset: start, vaddr: round_up(top, align), filesz: 0, memsz: 0, align };
            let at = ph.iter().rposition(|q| q.kind == PT_LOAD).map_or(ph.len(), |i| i + 1);
            ph.insert(at, p);
            (at, start)
        }
    };
    data.resize(start as usize, 0);
    let mut vaddrs = Vec::new();
    for b in blobs {
        data.resize(round_up(data.len() as u64, 8) as usize, 0);
        vaddrs.push(ph[seg].vaddr + (data.len() as u64 - ph[seg].offset));
        data.extend_from_slice(b);
    }
    let size = data.len() as u64 - ph[seg].offset;
    ph[seg].filesz = size;
    ph[seg].memsz = size;
    write_phdrs(data, off, &ph);
    Ok(vaddrs)
}

// Point the table entry at `slot` (its data pointer at +16, length at +24) to `vaddr`.
// In a position-independent binary the pointer is set at load time from a RELATIVE
// relocation's addend, so that is updated too; the slot itself is written for RELR and
// non-PIE binaries, which read the address from it.
pub fn point(data: &mut [u8], slot: usize, vaddr: u64, len: usize) -> Result<(), String> {
    let (_, ph) = phdrs(data)?;
    let ptr = slot + 16;
    let ptr_va = file_to_vaddr(&ph, ptr as u64).ok_or("table entry is not in a loaded segment")?;
    if let Some(dynp) = ph.iter().find(|p| p.kind == PT_DYNAMIC) {
        let (mut rela, mut relasz) = (None, 0);
        let mut o = dynp.offset as usize;
        while o + 16 <= (dynp.offset + dynp.filesz) as usize {
            match u64_at(data, o) { 0 => break, DT_RELA => rela = Some(u64_at(data, o + 8)), DT_RELASZ => relasz = u64_at(data, o + 8), _ => {} }
            o += 16;
        }
        if let Some(start) = rela.and_then(|va| vaddr_to_file(&ph, va)) {
            let end = (start + relasz as usize).min(data.len());
            for e in (start..end).step_by(24).filter(|&e| e + 24 <= end) {
                if u64_at(data, e) != ptr_va { continue; }
                if !RELATIVE.contains(&(u64_at(data, e + 8) as u32)) { return Err("the table entry's pointer has an unexpected relocation type".into()); }
                put_u64(data, e + 16, vaddr);
            }
        }
    }
    put_u64(data, ptr, vaddr);
    put_u64(data, slot + 24, len as u64);
    Ok(())
}
