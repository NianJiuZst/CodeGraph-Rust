//! Language detection (upstream `detectLanguage`, `EXTENSION_MAP`): extension map plus the
//! content checks that matter for the supported languages. Detection of unsupported
//! languages still works so `status` can report how much of a repo is outside the MVP set.

use cg_core::Lang;
use std::collections::BTreeMap;

/// Extension (lower-case, with dot) -> language. Mirrors upstream `EXTENSION_MAP` for the
/// languages `cg-core::Lang` knows.
pub fn language_for_extension(ext: &str) -> Lang {
    match ext {
        ".ts" | ".mts" | ".cts" => Lang::TypeScript,
        ".tsx" => Lang::Tsx,
        ".js" | ".mjs" | ".cjs" | ".xsjs" | ".xsjslib" => Lang::JavaScript,
        ".jsx" => Lang::Jsx,
        ".py" | ".pyw" => Lang::Python,
        ".go" => Lang::Go,
        ".rs" => Lang::Rust,
        ".java" => Lang::Java,
        ".kt" | ".kts" => Lang::Kotlin,
        ".c" | ".h" => Lang::C,
        ".cpp" | ".cc" | ".cxx" | ".hpp" | ".hxx" | ".metal" | ".cu" | ".cuh" => Lang::Cpp,
        ".cs" => Lang::CSharp,
        ".php" | ".module" | ".install" | ".theme" | ".inc" => Lang::Php,
        ".rb" | ".rake" => Lang::Ruby,
        ".swift" => Lang::Swift,
        ".dart" => Lang::Dart,
        ".scala" | ".sc" => Lang::Scala,
        ".lua" => Lang::Lua,
        ".luau" => Lang::Luau,
        ".r" => Lang::R,
        _ => Lang::Unknown,
    }
}

/// Lower-cased extension of the last path segment, including the dot ("" if none).
pub fn extension_of(path: &str) -> String {
    let name = path.rsplit('/').next().unwrap_or(path);
    match name.rfind('.') {
        Some(i) => name[i..].to_ascii_lowercase(),
        None => String::new(),
    }
}

/// Detect from the path alone (config `extensions` take precedence over the built-in map).
pub fn detect_from_path(path: &str, overrides: &BTreeMap<String, Lang>) -> Lang {
    let ext = extension_of(path);
    if let Some(l) = overrides.get(&ext) {
        return *l;
    }
    language_for_extension(&ext)
}

/// Detect with the file's text available: a `.js`/`.jsx` file carrying Flow's `@flow`
/// pragma is parsed with the TSX grammar, and a `.h` header with C++ constructs with the
/// C++ grammar, as upstream does.
pub fn detect(path: &str, source: Option<&str>, overrides: &BTreeMap<String, Lang>) -> Lang {
    let lang = detect_from_path(path, overrides);
    if matches!(lang, Lang::JavaScript | Lang::Jsx) {
        if let Some(src) = source {
            if has_flow_pragma(src) {
                return Lang::Tsx;
            }
        }
    }
    if lang == Lang::C && extension_of(path) == ".h" && !overrides.contains_key(".h") {
        if let Some(src) = source {
            if looks_like_cpp(src) {
                return Lang::Cpp;
            }
            if looks_like_objc(src) {
                return Lang::ObjC;
            }
            // Declaration-only C++ headers (`extern CComPtr<T> g;`, `void f(const T& x = 0);`, #3397).
            if looks_like_cpp_decl(src) {
                return Lang::Cpp;
            }
        }
    }
    lang
}

/// Whether a `.h` file is an Objective-C header (upstream `looksLikeObjc`): `@interface`,
/// `@implementation`, `@protocol` or `@synthesize` in the first 8 KiB. Such headers have no
/// walker here and must not be fed to the C grammar's error recovery.
pub fn looks_like_objc(source: &str) -> bool {
    use regex::Regex;
    use std::sync::LazyLock;
    static OBJC: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"@(?:interface|implementation|protocol|synthesize)\b").expect("static regex"));
    let mut end = source.len().min(8192);
    while !source.is_char_boundary(end) {
        end -= 1;
    }
    OBJC.is_match(&source[..end])
}

/// Whether a `.h` file contains constructs that are C++ and never valid C (upstream
/// `looksLikeCpp`): the first 8 KiB are checked for namespaces, classes, templates, access
/// sections, `virtual` and `using`; the whole file (comments removed) for a class/struct
/// base clause (`struct D : Base {`), whose only C++ signal may sit past a long C preamble.
pub fn looks_like_cpp(source: &str) -> bool {
    use regex::Regex;
    use std::sync::LazyLock;
    static SAMPLE: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(
            r"\bnamespace\b|\bclass\s+\w+\s*[:{]|\b(?:class|struct)\s+[A-Z][A-Z0-9_]+\s+\w+\s*(?:final\s*)?[:{]|\btemplate\s*<|\b(?:public|private|protected)\s*:|\bvirtual\b|\busing\s+(?:namespace\b|\w+\s*=)",
        )
        .expect("static regex")
    });
    static BASE_CLAUSE: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(r"\b(?:class|struct)\s+\w+\s*(?:final\s*)?:\s*(?:(?:public|protected|private|virtual)\s+)*[A-Za-z_][\w:]*(?:\s*<[^{};]*>)?\s*[{,]")
            .expect("static regex")
    });
    static COMMENT: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?s)/\*.*?\*/|//[^\n]*").expect("static regex"));
    let mut end = source.len().min(8192);
    while !source.is_char_boundary(end) {
        end -= 1;
    }
    if SAMPLE.is_match(&source[..end]) {
        return true;
    }
    BASE_CLAUSE.is_match(&COMMENT.replace_all(source, " "))
}

/// Declaration-only C++ `.h` headers (upstream #3397): C++ syntax in headers that have no
/// `class` / `namespace` / `template<` keyword (e.g. `const T&`, `T const&`, `extern CComPtr<X> g;`,
/// or function prototypes with reference / default / template parameters). Skips Objective-C files
/// (`#import` or `@` in code) whose generics share these shapes.
pub fn looks_like_cpp_decl(source: &str) -> bool {
    use regex::Regex;
    use std::sync::LazyLock;
    if source.contains("#import") {
        return false;
    }
    // Strip comments, string/char literals, and preprocessor directives into a code-only view.
    let mut code = String::with_capacity(source.len());
    let bytes = source.as_bytes();
    let mut i = 0;
    let mut line_start = true;
    while i < bytes.len() {
        let b = bytes[i];
        if line_start && (b == b' ' || b == b'\t') {
            code.push(' ');
            i += 1;
            continue;
        }
        if line_start && b == b'#' {
            // Skip preprocessor line (including `\` continuations)
            while i < bytes.len() {
                if bytes[i] == b'\n' {
                    let prev_bs = i > 0 && (bytes[i - 1] == b'\\' || (i > 1 && bytes[i - 1] == b'\r' && bytes[i - 2] == b'\\'));
                    code.push('\n');
                    i += 1;
                    if !prev_bs {
                        break;
                    }
                } else {
                    i += 1;
                }
            }
            line_start = true;
            continue;
        }
        if b == b'/' && i + 1 < bytes.len() && bytes[i + 1] == b'/' {
            while i < bytes.len() && bytes[i] != b'\n' {
                i += 1;
            }
            continue;
        }
        if b == b'/' && i + 1 < bytes.len() && bytes[i + 1] == b'*' {
            i += 2;
            while i + 1 < bytes.len() && !(bytes[i] == b'*' && bytes[i + 1] == b'/') {
                if bytes[i] == b'\n' {
                    code.push('\n');
                }
                i += 1;
            }
            i = (i + 2).min(bytes.len());
            code.push(' ');
            continue;
        }
        if b == b'"' || b == b'\'' {
            let q = b;
            i += 1;
            while i < bytes.len() {
                if bytes[i] == b'\\' {
                    i = (i + 2).min(bytes.len());
                } else if bytes[i] == q {
                    i += 1;
                    break;
                } else {
                    if bytes[i] == b'\n' {
                        code.push('\n');
                    }
                    i += 1;
                }
            }
            code.push(' ');
            line_start = false;
            continue;
        }
        if b == b'@' {
            return false; // Objective-C
        }
        if b == b'\n' {
            line_start = true;
        } else if b != b'\r' {
            line_start = false;
        }
        code.push(b as char);
        i += 1;
    }
    static CONST_REF: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(r"\bconst\s+(?:(?:struct|enum|union)\s+)?[A-Za-z_][\w:]*(?:\s*<[^{};]*>)?\s*&|\b[A-Za-z_][\w:]*\s+const\s*&").expect("static regex")
    });
    static TEMPLATE_DECL: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(r"(?:^|[;{}])\s*(?:(?:extern|static|inline|constexpr|const|volatile)\s+)*[A-Za-z_][\w:]*\s*<\s*[A-Za-z_][\w:\s,*&<>]*>\s*[&*]?\s*[A-Za-z_]\w*\s*[=;(\[]")
            .expect("static regex")
    });
    static FN_CPP_PARAM: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(
            r"(?:^|[;{}])\s*(?:(?:extern|static|inline|constexpr|const|unsigned|signed|long|short|struct)\s+)*[A-Za-z_][\w:]*(?:\s*<[^{};]*>)?[\s*&]+[A-Za-z_]\w*\s*\(([^()]*)\)\s*(?:const\s*)?(?:noexcept\s*)?[;{]",
        )
        .expect("static regex")
    });
    static PARAM_REF_OR_DEFAULT: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r"(?:^|,)\s*(?:const\s+)?[A-Za-z_][\w:]*(?:\s*<[^{};,]*>)?\s*(?:&|\*?\s*[A-Za-z_]\w*\s*=)").expect("static regex"));
    if CONST_REF.is_match(&code) || TEMPLATE_DECL.is_match(&code) {
        return true;
    }
    for cap in FN_CPP_PARAM.captures_iter(&code) {
        if let Some(params) = cap.get(1) {
            let p = params.as_str();
            if (p.contains('&') || p.contains('=') || p.contains('<')) && PARAM_REF_OR_DEFAULT.is_match(p) {
                return true;
            }
        }
    }
    false
}

/// Whether leading comments carry `@flow` (and not `@noflow`).
pub fn has_flow_pragma(source: &str) -> bool {
    let mut end = source.len().min(4096);
    while !source.is_char_boundary(end) {
        end -= 1;
    }
    let mut head = &source[..end];
    if head.starts_with("#!") {
        head = head.find('\n').map(|i| &head[i + 1..]).unwrap_or("");
    }
    // Collect the run of leading comments.
    let mut lead_end = 0usize;
    let mut rest = head;
    loop {
        let trimmed = rest.trim_start();
        let skipped = rest.len() - trimmed.len();
        if trimmed.starts_with("//") {
            let line_len = trimmed.find('\n').unwrap_or(trimmed.len());
            lead_end += skipped + line_len;
            rest = &trimmed[line_len..];
        } else if trimmed.starts_with("/*") {
            match trimmed.find("*/") {
                Some(i) => {
                    lead_end += skipped + i + 2;
                    rest = &trimmed[i + 2..];
                }
                None => break,
            }
        } else {
            break;
        }
    }
    let lead = &head[..lead_end.min(head.len())];
    let has = |word: &str| lead.match_indices(word).any(|(i, _)| lead[i + word.len()..].chars().next().is_none_or(|c| !(c.is_alphanumeric() || c == '_')));
    has("@flow") && !has("@noflow")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extensions() {
        let none = BTreeMap::new();
        assert_eq!(detect_from_path("a/b/c.TS", &none), Lang::TypeScript);
        assert_eq!(detect_from_path("x.mjs", &none), Lang::JavaScript);
        assert_eq!(detect_from_path("x.kts", &none), Lang::Kotlin);
        assert_eq!(detect_from_path("Makefile", &none), Lang::Unknown);
        let mut o = BTreeMap::new();
        o.insert(".tpl".to_string(), Lang::Php);
        assert_eq!(detect_from_path("a.tpl", &o), Lang::Php);
    }

    #[test]
    fn cpp_headers() {
        let none = BTreeMap::new();
        assert!(looks_like_cpp("#pragma once\nnamespace a { int f(); }\n"));
        assert!(looks_like_cpp("class Foo {\npublic:\n  void f();\n};\n"));
        assert!(looks_like_cpp("template <typename T> T id(T t);\n"));
        assert!(looks_like_cpp("struct MYLIB_API Thing : Base {};\n"));
        // the only signal is a base clause past the 8 KiB sample
        let long = format!("{}\nstruct Derived : public ns::Base<int> {{ int x; }};\n", "#define X 1\n".repeat(1000));
        assert!(looks_like_cpp(&long));
        assert!(!looks_like_cpp("/* struct foo: x, y */\nstruct foo { int x : 3; };\nint g(struct foo *p);\n"));
        assert!(!looks_like_cpp("#include <stdio.h>\nint main(void) { return 0; }\n"));
        assert_eq!(detect("a/b.h", Some("namespace x {}"), &none), Lang::Cpp);
        assert_eq!(detect("a/b.h", Some("int x;"), &none), Lang::C);
        assert_eq!(detect("a/b.h", Some("#import <Foundation/Foundation.h>\n@interface Foo : NSObject\n@end\n"), &none), Lang::ObjC);
        assert!(!Lang::ObjC.is_supported());
        assert_eq!(detect("a/b.c", Some("namespace x {}"), &none), Lang::C);
        let mut o = BTreeMap::new();
        o.insert(".h".to_string(), Lang::C);
        assert_eq!(detect("a/b.h", Some("namespace x {}"), &o), Lang::C);
    }

    #[test]
    fn flow_pragma() {
        assert!(has_flow_pragma("// @flow\nconst a = 1;"));
        assert!(has_flow_pragma("/* hello */\n/** @flow strict */\nx"));
        assert!(!has_flow_pragma("// @noflow\nx"));
        assert!(!has_flow_pragma("const a = 1; // @flow"));
        assert_eq!(detect("a.js", Some("// @flow\n"), &BTreeMap::new()), Lang::Tsx);
    }
}
