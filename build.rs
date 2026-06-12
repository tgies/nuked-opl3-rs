fn main() {
    #[cfg(feature = "c-reference-tests")]
    {
        use std::env;
        use std::path::PathBuf;

        println!("cargo:rerun-if-changed=src/nuked-opl3/opl3.c");
        println!("cargo:rerun-if-changed=src/nuked-opl3/opl3.h");
        println!("cargo:rerun-if-env-changed=OPL3_RS_C_REF_ROOT");

        let stereo_ext = env::var_os("CARGO_FEATURE_STEREO_EXT").is_some();
        let target = env::var("TARGET").expect("TARGET must be set");
        let c_ref_root = env::var_os("OPL3_RS_C_REF_ROOT")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("src/nuked-opl3"));
        let c_ref_source = c_ref_root.join("opl3.c");

        let mut build = cc::Build::new();
        build.include(&c_ref_root).file(&c_ref_source).opt_level(2);
        if !target.contains("msvc") {
            build.std("c99");
        }
        if stereo_ext {
            build.define("OPL_ENABLE_STEREOEXT", "1");
            build.define("M_PI", "3.14159265358979323846");
        }
        build.compile("opl3_ref");

        if stereo_ext && !target.contains("windows") {
            println!("cargo:rustc-link-lib=m");
        }
    }
}
