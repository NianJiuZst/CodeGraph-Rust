//! Grammar registry: codegraph `Language` string -> native tree-sitter grammar.
//!
//! P1: each language sits behind a cargo feature; lua, scala, dart and r are
//! not vendored. `tsx` and `jsx` reuse another language's grammar as upstream does.

use tree_sitter::Language;

// Vendored kotlin grammar (build.rs-compiled C, fwcd main@d6af96d).
#[cfg(feature = "lang-kotlin")]
extern "C" {
    fn tree_sitter_kotlin() -> *const ();
}
// Vendored swift grammar (build.rs-compiled C, crate 0.7.4 src/ + scanner patch).
#[cfg(feature = "lang-swift")]
extern "C" {
    fn tree_sitter_swift() -> *const ();
}

/// Languages this kernel build can extract.
pub const LANGUAGES: &[&str] = &[
    #[cfg(feature = "lang-ts")]
    "typescript",
    #[cfg(feature = "lang-ts")]
    "tsx",
    #[cfg(feature = "lang-ts")]
    "javascript",
    #[cfg(feature = "lang-ts")]
    "jsx",
    #[cfg(feature = "lang-java")]
    "java",
    #[cfg(feature = "lang-python")]
    "python",
    #[cfg(feature = "lang-go")]
    "go",
    #[cfg(feature = "lang-rust")]
    "rust",
    #[cfg(feature = "lang-kotlin")]
    "kotlin",
    #[cfg(feature = "lang-c")]
    "c",
    #[cfg(feature = "lang-c")]
    "cpp",
    #[cfg(feature = "lang-csharp")]
    "csharp",
    #[cfg(feature = "lang-php")]
    "php",
    #[cfg(feature = "lang-ruby")]
    "ruby",
    #[cfg(feature = "lang-swift")]
    "swift",
];

pub fn grammar_for(language: &str) -> Option<Language> {
    match language {
        #[cfg(feature = "lang-ts")]
        "typescript" => Some(tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into()),
        #[cfg(feature = "lang-ts")]
        "tsx" => Some(tree_sitter_typescript::LANGUAGE_TSX.into()),
        #[cfg(feature = "lang-ts")]
        "javascript" | "jsx" => Some(tree_sitter_javascript::LANGUAGE.into()),
        #[cfg(feature = "lang-java")]
        "java" => Some(tree_sitter_java::LANGUAGE.into()),
        #[cfg(feature = "lang-python")]
        "python" => Some(tree_sitter_python::LANGUAGE.into()),
        #[cfg(feature = "lang-go")]
        "go" => Some(tree_sitter_go::LANGUAGE.into()),
        #[cfg(feature = "lang-rust")]
        "rust" => Some(tree_sitter_rust::LANGUAGE.into()),
        #[cfg(feature = "lang-kotlin")]
        "kotlin" => {
            Some(unsafe { tree_sitter_language::LanguageFn::from_raw(tree_sitter_kotlin) }.into())
        }
        #[cfg(feature = "lang-c")]
        "c" => Some(tree_sitter_c::LANGUAGE.into()),
        #[cfg(feature = "lang-c")]
        "cpp" => Some(tree_sitter_cpp::LANGUAGE.into()),
        #[cfg(feature = "lang-csharp")]
        "csharp" => Some(tree_sitter_c_sharp::LANGUAGE.into()),
        #[cfg(feature = "lang-ruby")]
        "ruby" => Some(tree_sitter_ruby::LANGUAGE.into()),
        // The full HTML-interleaving variant, never LANGUAGE_PHP_ONLY.
        #[cfg(feature = "lang-php")]
        "php" => Some(tree_sitter_php::LANGUAGE_PHP.into()),
        #[cfg(feature = "lang-swift")]
        "swift" => {
            Some(unsafe { tree_sitter_language::LanguageFn::from_raw(tree_sitter_swift) }.into())
        }
        _ => None,
    }
}
