fn main() {
    // P1: napi_build::setup() removed; only vendored Kotlin and Swift grammars
    // are compiled here (lua, scala and dart are not vendored).
    if std::env::var_os("CARGO_FEATURE_LANG_KOTLIN").is_some() {
        // Kotlin grammar — vendored C (fwcd/tree-sitter-kotlin main@d6af96d,
        // upstream #3410). c11 required for scanner _Static_assert on MSVC.
        let mut c = cc::Build::new();
        c.std("c11");
        c.include("grammars/kotlin");
        c.file("grammars/kotlin/parser.c");
        c.file("grammars/kotlin/scanner.c");
        c.flag_if_supported("-Wno-unused-parameter");
        c.flag_if_supported("-Wno-unused-but-set-variable");
        c.flag_if_supported("-Wno-trigraphs");
        c.flag_if_supported("-utf-8"); // msvc
        c.compile("tree-sitter-kotlin");
        println!("cargo:rerun-if-changed=grammars/kotlin");
    }
    if std::env::var_os("CARGO_FEATURE_LANG_SWIFT").is_some() {
        // Swift grammar — vendored C (tree-sitter-swift 0.7.4 + upstream scanner patch).
        let mut swift = cc::Build::new();
        swift.std("c11");
        swift.include("grammars/swift");
        swift.file("grammars/swift/parser.c");
        swift.file("grammars/swift/scanner.c");
        swift.flag_if_supported("-Wno-unused-parameter");
        swift.flag_if_supported("-Wno-unused-but-set-variable");
        swift.flag_if_supported("-Wno-trigraphs");
        swift.flag_if_supported("-utf-8"); // msvc
        swift.compile("tree-sitter-swift");
        println!("cargo:rerun-if-changed=grammars/swift");
    }
    println!("cargo:rerun-if-changed=build.rs");
}
