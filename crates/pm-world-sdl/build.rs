//! Compile the WGSL shaders to SPIR-V at build time with naga (a library
//! build-dependency — no global shader toolchain to install, and the
//! generated .spv never goes stale or gets committed).

fn main() {
    let out_dir = std::env::var("OUT_DIR").unwrap();
    let files: [(&str, &[(&str, naga::ShaderStage)]); 3] = [
        (
            "shaders/basic3d.wgsl",
            &[
                ("vs_main", naga::ShaderStage::Vertex),
                ("vs_inst", naga::ShaderStage::Vertex),
                ("fs_main", naga::ShaderStage::Fragment),
            ],
        ),
        (
            "shaders/post3d.wgsl",
            &[("cs_post", naga::ShaderStage::Compute)],
        ),
        (
            "shaders/text.wgsl",
            &[("cs_text", naga::ShaderStage::Compute)],
        ),
    ];
    for (file, entries) in files {
        println!("cargo:rerun-if-changed={file}");
        let src = std::fs::read_to_string(file).unwrap_or_else(|e| panic!("read {file}: {e}"));
        let module = naga::front::wgsl::parse_str(&src).unwrap_or_else(|e| {
            panic!("{file} WGSL parse error:\n{}", e.emit_to_string(&src));
        });
        let info = naga::valid::Validator::new(
            naga::valid::ValidationFlags::all(),
            naga::valid::Capabilities::all(),
        )
        .validate(&module)
        .unwrap_or_else(|e| panic!("{file} validation: {e:?}"));

        let opts = naga::back::spv::Options::default();
        for &(entry, stage) in entries {
            let pipeline = naga::back::spv::PipelineOptions {
                shader_stage: stage,
                entry_point: entry.to_string(),
            };
            let words = naga::back::spv::write_vec(&module, &info, &opts, Some(&pipeline))
                .expect("SPIR-V emit");
            let bytes: Vec<u8> = words.iter().flat_map(|w| w.to_le_bytes()).collect();
            std::fs::write(format!("{out_dir}/{entry}.spv"), bytes).expect("write spv");
        }
    }
}
