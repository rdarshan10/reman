fn main() {
    // Windows (MSVC): reman.exe.manifest, embedded as the exe's manifest resource. It opts the
    // daemon into the segment heap. Written as a .res file here, so linking needs no SDK tools.
    println!("cargo:rerun-if-changed=reman.exe.manifest");
    let os = std::env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();
    let env = std::env::var("CARGO_CFG_TARGET_ENV").unwrap_or_default();
    if os != "windows" || env != "msvc" {
        return;
    }
    let dir = std::path::PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").unwrap());
    let manifest = std::fs::read(dir.join("reman.exe.manifest")).expect("reading reman.exe.manifest");
    let res = std::path::PathBuf::from(std::env::var("OUT_DIR").unwrap()).join("reman-manifest.res");
    std::fs::write(&res, manifest_res(&manifest)).expect("writing the manifest resource");
    println!("cargo:rustc-link-arg-bin=reman={}", res.display());
}

/// A .res file holding one resource: RT_MANIFEST (24), id 1, after the empty entry every .res
/// file starts with.
fn manifest_res(data: &[u8]) -> Vec<u8> {
    fn entry(out: &mut Vec<u8>, kind: u16, id: u16, flags: u16, lang: u16, data: &[u8]) {
        out.extend((data.len() as u32).to_le_bytes()); // DataSize
        out.extend(32u32.to_le_bytes()); // HeaderSize
        out.extend([0xFFFFu16, kind, 0xFFFF, id].iter().flat_map(|w| w.to_le_bytes())); // TYPE, NAME
        out.extend(0u32.to_le_bytes()); // DataVersion
        out.extend(flags.to_le_bytes()); // MemoryFlags
        out.extend(lang.to_le_bytes()); // LanguageId
        out.extend(0u32.to_le_bytes()); // Version
        out.extend(0u32.to_le_bytes()); // Characteristics
        out.extend(data);
        while out.len() % 4 != 0 {
            out.push(0);
        }
    }
    let mut out = Vec::new();
    entry(&mut out, 0, 0, 0, 0, &[]);
    entry(&mut out, 24, 1, 0x1030, 0x0409, data);
    out
}
