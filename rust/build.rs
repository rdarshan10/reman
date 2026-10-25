fn main() {
    tldr_map();
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

/// tldr_map.json (tools/build_tldr_map.py) as describe.rs reads it in place, never copying its
/// text onto the heap: a `C\tpage\tsentence` line per description, then an
/// `E\tpage\tdescription\twords\t0|1` line per example (words space-separated), a page's
/// examples together and in order.
fn tldr_map() {
    println!("cargo:rerun-if-changed=../tldr_map.json");
    let dir = std::path::PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").unwrap());
    let json = std::fs::read(dir.join("..").join("tldr_map.json")).expect("reading tldr_map.json");
    let map: serde_json::Value = serde_json::from_slice(&json).expect("parsing tldr_map.json");
    let clean = |s: &str| s.replace(['\t', '\n', '\r'], " ");
    let mut out = String::with_capacity(json.len());
    for (page, sentence) in map["cmd"].as_object().expect("cmd") {
        out += &format!("C\t{}\t{}\n", clean(page), clean(sentence.as_str().unwrap_or("")));
    }
    for (page, examples) in map["ex"].as_object().expect("ex") {
        for e in examples.as_array().into_iter().flatten() {
            let words: Vec<String> = e[1].as_array().into_iter().flatten().filter_map(|w| w.as_str()).map(|w| clean(w).replace(' ', "")).collect();
            out += &format!("E\t{}\t{}\t{}\t{}\n", clean(page), clean(e[0].as_str().unwrap_or("")), words.join(" "), e[2].as_u64().unwrap_or(0).min(1));
        }
    }
    let path = std::path::PathBuf::from(std::env::var("OUT_DIR").unwrap()).join("tldr_map.tsv");
    std::fs::write(path, out).expect("writing tldr_map.tsv");
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
