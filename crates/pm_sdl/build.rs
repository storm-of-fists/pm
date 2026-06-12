//! Compile the WGSL shaders to SPIR-V at build time with naga (a library
//! build-dependency — no global shader toolchain to install, and the
//! generated .spv never goes stale or gets committed).

fn main() {
    println!("cargo:rerun-if-changed=shaders/basic3d.wgsl");
    let src = std::fs::read_to_string("shaders/basic3d.wgsl").expect("read solids.wgsl");
    let module = naga::front::wgsl::parse_str(&src).unwrap_or_else(|e| {
        panic!("WGSL parse error:\n{}", e.emit_to_string(&src));
    });
    let info = naga::valid::Validator::new(
        naga::valid::ValidationFlags::all(),
        naga::valid::Capabilities::empty(),
    )
    .validate(&module)
    .expect("WGSL validation");

    let out_dir = std::env::var("OUT_DIR").unwrap();
    let opts = naga::back::spv::Options::default();
    for (entry, stage) in
        [("vs_main", naga::ShaderStage::Vertex), ("fs_main", naga::ShaderStage::Fragment)]
    {
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
