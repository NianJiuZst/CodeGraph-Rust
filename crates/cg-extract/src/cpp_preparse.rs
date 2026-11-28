//! C++ source pre-processing before tree-sitter: a port of upstream's `preParseCppSource`
//! (`src/extraction/languages/c-cpp.ts`). The grammar does not know macros, so `class FMT_API x {`,
//! a lone `FMT_BEGIN_NAMESPACE`, `FMT_CONSTEXPR auto f()` and thread-safety annotations send it
//! into error recovery, and classes collapse into loose functions.
//!
//! Every pass blanks tokens in place, keeping line endings, so byte offsets stay those of the
//! original text. The patterns upstream writes with lookaround and backreferences are spelled
//! here as scanners, with the same backtracking order; a run of macros is retried from longest
//! to shortest. Offsets count bytes where upstream counts UTF-16 units.

use memchr::memmem;
use regex::bytes::Regex;
use std::sync::LazyLock;

/// `(?-u)`: `\w`, `\s` and `\b` are ASCII, as in JavaScript.
macro_rules! re {
    ($($name:ident = $pat:expr;)*) => {
        $(static $name: LazyLock<Regex> = LazyLock::new(|| Regex::new(&format!("(?-u){}", $pat)).unwrap());)*
    };
}

const PAREN_KEYWORDS: &[&[u8]] = &[
    b"if",
    b"while",
    b"for",
    b"switch",
    b"catch",
    b"return",
    b"sizeof",
    b"alignof",
    b"alignas",
    b"decltype",
    b"typeid",
    b"noexcept",
    b"throw",
    b"static_assert",
    b"new",
    b"delete",
    b"case",
    b"requires",
    b"co_await",
    b"co_yield",
    b"co_return",
    b"asm",
    b"__asm__",
    b"__attribute__",
    b"__attribute",
    b"__declspec",
    b"_Pragma",
    b"else",
    b"do",
    b"constexpr",
    b"consteval",
    b"explicit",
    b"typeof",
    b"__typeof__",
    b"__typeof",
    b"_Alignof",
    b"__alignof__",
    b"volatile",
    b"__volatile__",
];
const TYPE_KEYWORDS: &[&[u8]] = &[
    b"void",
    b"bool",
    b"char",
    b"char8_t",
    b"char16_t",
    b"char32_t",
    b"wchar_t",
    b"short",
    b"int",
    b"long",
    b"float",
    b"double",
    b"signed",
    b"unsigned",
    b"auto",
];
const OTHER_KEYWORDS: &[&[u8]] = &[
    b"const",
    b"volatile",
    b"static",
    b"extern",
    b"inline",
    b"virtual",
    b"explicit",
    b"constexpr",
    b"consteval",
    b"constinit",
    b"friend",
    b"mutable",
    b"thread_local",
    b"register",
    b"typedef",
    b"using",
    b"template",
    b"typename",
    b"class",
    b"struct",
    b"union",
    b"enum",
    b"namespace",
    b"operator",
    b"goto",
    b"break",
    b"continue",
    b"default",
    b"public",
    b"private",
    b"protected",
    b"this",
    b"true",
    b"false",
    b"nullptr",
    b"and",
    b"or",
    b"not",
    b"xor",
    b"bitand",
    b"bitor",
    b"compl",
    b"and_eq",
    b"or_eq",
    b"xor_eq",
    b"not_eq",
];
const OPERATOR_WORDS: &[&[u8]] = &[b"and", b"and_eq", b"bitand", b"bitor", b"compl", b"not", b"not_eq", b"or", b"or_eq", b"xor", b"xor_eq"];
/// Keywords that can't follow a type name, so what stands before them is no type.
const DECL_KEYWORDS: &[&[u8]] = &[
    b"static",
    b"inline",
    b"virtual",
    b"explicit",
    b"constexpr",
    b"consteval",
    b"constinit",
    b"friend",
    b"extern",
    b"thread_local",
    b"mutable",
    b"typedef",
    b"using",
    b"template",
    b"typename",
    b"class",
    b"struct",
    b"union",
    b"enum",
    b"void",
    b"bool",
    b"char",
    b"char8_t",
    b"char16_t",
    b"char32_t",
    b"wchar_t",
    b"short",
    b"int",
    b"long",
    b"float",
    b"double",
    b"signed",
    b"unsigned",
    b"auto",
    b"decltype",
    b"return",
];
/// Curated inline-specifier macros: an arbitrary all-caps token may be a real return type.
const INLINE_MACROS: &[&str] = &[
    "FORCEINLINE_DEBUGGABLE",
    "FORCENOINLINE",
    "FORCEINLINE",
    "PUGI__FN_NO_INLINE",
    "PUGI__FN",
    "PUGIXML_FUNCTION",
    "_ALWAYS_INLINE_",
    "_FORCE_INLINE_",
    "BOOST_FORCEINLINE",
    "BOOST_NOINLINE",
    "Q_INVOKABLE",
    "Q_SCRIPTABLE",
    "Q_ALWAYS_INLINE",
    "Q_SLOT",
    "Q_SIGNAL",
    "FOLLY_ALWAYS_INLINE",
    "FOLLY_NOINLINE",
    "ABSL_ATTRIBUTE_ALWAYS_INLINE",
    "ABSL_ATTRIBUTE_NOINLINE",
    "LLVM_ATTRIBUTE_ALWAYS_INLINE",
    "LLVM_ATTRIBUTE_NOINLINE",
    "V8_INLINE",
    "V8_NOINLINE",
    "EIGEN_STRONG_INLINE",
    "EIGEN_ALWAYS_INLINE",
    "EIGEN_DEVICE_FUNC",
    "RAPIDJSON_FORCEINLINE",
    "MOZ_ALWAYS_INLINE",
    "MOZ_NEVER_INLINE",
    "PROTOBUF_ALWAYS_INLINE",
    "PROTOBUF_NOINLINE",
    "FMT_CONSTEXPR20",
    "FMT_CONSTEXPR",
    "FMT_INLINE",
    "JSON_HEDLEY_ALWAYS_INLINE",
    "JSON_HEDLEY_NEVER_INLINE",
    "HEDLEY_ALWAYS_INLINE",
    "HEDLEY_NEVER_INLINE",
    "GLM_FUNC_QUALIFIER",
    "GLM_FUNC_DECL",
    "GLM_CONSTEXPR",
    "GLM_INLINE",
    "SIMD_FORCE_INLINE",
    "SK_ALWAYS_INLINE",
    "CV_ALWAYS_INLINE",
    "CV_INLINE",
    "EA_FORCE_INLINE",
    "EA_NOINLINE",
    "CC_INLINE",
    "NEVER_INLINE",
    "G_INLINE_FUNC",
    "SQLITE_PRIVATE",
    "SQLITE_API",
    "STDMETHODCALLTYPE",
    "WINAPIV",
    "WINAPI",
    "APIENTRY",
    "ALWAYS_INLINE",
    "FORCE_INLINE",
    "NOINLINE",
];

const TYPE_ARGS: &str = r"<(?:[^<>;{}]|<(?:[^<>;{}]|<[^<>;{}]*>)*>)*>";

re! {
    CLASS_KEYWORD = r"\b(?:class|struct)\s+";
    EXPORT_PRE = r"\b(?:class|struct)\s+[A-Z][A-Z0-9_]";
    POINTER_PRE = r"[*&]\s*[A-Z][A-Z0-9]*_";
    TRAILING_PRE = r"\)[\s\w&]*?\b[A-Z][A-Z0-9_]{2,}\b";
    DECLARATOR_PRE = r"[\w\]]\s+[A-Z][A-Z0-9]*_[A-Z0-9_]*\s*[(;=,)]";
    LEADING_PRE = r"(?mR)(?:^|\]\])[ \t]*[A-Z][A-Z0-9]*_[A-Z0-9_]*\s";
    BLANK_LINE = r"\n[ \t]*\r?\n";
    OPERATOR_TAIL = r"\boperator\s*(?:\(\s*\)|\[\s*\]|[^\s\w()\[\]]{1,3}|\s+[A-Za-z_][\w\s:<>*&]*)\s*$";
    NAME_TAIL = r"([A-Za-z_]\w*)\s*$";
    TYPE_THEN_NAME = format!(
        r"\A(?:::\s*)?[A-Za-z_]\w*(?:\s*{TYPE_ARGS})?(?:\s*::\s*[A-Za-z_]\w*(?:\s*{TYPE_ARGS})?)*(?:\s*(?:const|volatile)\b)?(?:\s*[*&]+\s*|\s+)(?:(?:const|volatile)\s+)?(?:(?:::\s*)?~?[A-Za-z_]\w*(?:\s*::\s*~?[A-Za-z_]\w*)*\s*[(;=\[{{]|operator\b)"
    );
    C_LEADING = r"(?mR)^[ \t]*([A-Z][A-Z0-9_]{2,})\s+[A-Za-z_]\w*[\s*]+[A-Za-z_]\w*\s*\(";
    INLINE_MACRO = format!(r"\b({})\b\s+[A-Za-z_]", INLINE_MACROS.join("|"));
    API_PREFIX = r"\b([A-Z][A-Z0-9_]*(?:_API|_EXPORT|_ABI))\b\s+[A-Za-z_]";
    INLINE_ANNOTATION_PRE = r"\b(?:UMETA|UPARAM|UE_DEPRECATED)";
    INLINE_ANNOTATION = r"\b(?:UMETA|UPARAM|UE_DEPRECATED\w*)\s*\(";
    ANNOTATION_CALL = r"(?mR)^[ \t]*([A-Z][A-Z0-9_]{2,})\s*\(";
    LONE_PRE = r"(?mR)^[ \t]*[A-Z][A-Z0-9_]{3,}[ \t]*\r?$";
    LONE_LINE = r"^[ \t]*([A-Z][A-Z0-9_]{3,})[ \t]*(?://[^\n\r]*|/\*[^\n\r]*\*/[ \t]*)?\r?$";
    COMMENT_IN_LINE = r"/\*[^\r\n]*?\*/|//[^\r\n]*$";
    TEMPLATE_HEAD = r"^template\s*<";
    COM_ALIAS = r"(?mR)^[ \t]*#[ \t]*define[ \t]+interface[ \t]+struct\b";
    COM_DECLARATION = r"(?mR)(?:^|[;{}])\s*\binterface\s+[A-Za-z_]\w*\s*(:[^;{}]+)?\{";
    VIRTUAL = r"\bvirtual\b";
    ACCESS_MACRO = r"(?m)^([ \t]*)([A-Z][A-Z0-9]*_[A-Z0-9_]*)([ \t]*):[ \t]*(?://[^\n]*)?\r?$";
    LINE_COMMENT_END = r"//[^\r\n]*$";
    BLOCK_COMMENT_INLINE = r"/\*[^\r\n]*?\*/";
    DIRECTIVE_NAME = r"^[ \t]*#[ \t]*(\w+)";
    CONDITIONAL = r"^(?:if|ifdef|ifndef|else|elif|elifdef|elifndef|endif)$";
    ENDS_STATEMENT = r"[;{}]$|^[\w\s]*:$|\*/$";
    ACCESS_LABEL = r"^(?:public|private|protected)\s*:$";
    STATEMENT_MACRO = r"^[A-Z][A-Z0-9_]*(?:\s*\([^\r\n]*\))?$";
    FOUNDATION_PRE = r"\b(?:NS|CF)_[A-Z_]*(?:ENUM|OPTIONS)\s*\(";
    FOUNDATION_ENUM = r"\b(?:(typedef)(\s+))?(NS_ENUM|NS_OPTIONS|NS_CLOSED_ENUM|NS_ERROR_ENUM|CF_ENUM|CF_OPTIONS|CF_CLOSED_ENUM)(\s*)\(([^(),]*),(\s*)([A-Za-z_]\w*)(\s*)\)";
    METAL_ATTRIBUTE = r"\[\[\s*[A-Za-z_]\w*(?:\s*\([^()\n]*\))?(?:\s*,\s*[A-Za-z_]\w*(?:\s*\([^()\n]*\))?)*\s*\]\]";
    CUDA_LAUNCH_BOUNDS = r"\b__launch_bounds__\s*\([^()\n]*\)";
    CUDA_SPECIFIER = r"\b__(?:global|device|host|constant|shared|managed|grid_constant|forceinline|noinline|launch_bounds)__\b";
    CUDA_LAUNCH_CONFIG = r"<<<[^;]{0,400}?>>>";
}

fn is_word(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_'
}

fn is_space(b: u8) -> bool {
    matches!(b, b' ' | b'\t' | b'\n' | b'\r' | 0x0b | 0x0c)
}

fn is_eol(b: u8) -> bool {
    b == b'\n' || b == b'\r'
}

/// The byte at `i`, or NUL past the end (no scanner below looks for NUL).
fn at(c: &[u8], i: usize) -> u8 {
    c.get(i).copied().unwrap_or(0)
}

fn skip_space(c: &[u8], mut i: usize) -> usize {
    while i < c.len() && is_space(c[i]) {
        i += 1;
    }
    i
}

/// Overwrite `[from, to)` with `byte`, keeping line endings.
fn fill(s: &mut [u8], from: usize, to: usize, byte: u8) {
    for b in &mut s[from..to] {
        if !is_eol(*b) {
            *b = byte;
        }
    }
}

/// The end of the identifier at `i`.
fn ident(c: &[u8], i: usize) -> Option<usize> {
    let first = at(c, i);
    if !(first.is_ascii_alphabetic() || first == b'_') {
        return None;
    }
    let mut j = i + 1;
    while j < c.len() && is_word(c[j]) {
        j += 1;
    }
    Some(j)
}

/// The end of the word (`\w+`) at `i`; `i` itself when there is none.
fn word_end(c: &[u8], mut i: usize) -> usize {
    while i < c.len() && is_word(c[i]) {
        i += 1;
    }
    i
}

fn caps_end(c: &[u8], i: usize) -> Option<usize> {
    if !at(c, i).is_ascii_uppercase() {
        return None;
    }
    let mut j = i + 1;
    while j < c.len() && (c[j].is_ascii_uppercase() || c[j].is_ascii_digit() || c[j] == b'_') {
        j += 1;
    }
    Some(j)
}

/// `[A-Z][A-Z0-9]*_[A-Z0-9_]*` at `i`, however it is delimited.
fn underscore_macro(c: &[u8], i: usize) -> Option<usize> {
    caps_end(c, i).filter(|j| c[i..*j].contains(&b'_'))
}

/// `[A-Z][A-Z0-9_]{min-1,}\b` at `i`.
fn caps_token(c: &[u8], i: usize, min: usize) -> Option<usize> {
    caps_end(c, i).filter(|j| *j - i >= min && !is_word(at(c, *j)))
}

/// Just past the `)` closing the `(` at `open`, nesting at most `depth` levels.
fn balanced(c: &[u8], open: usize, depth: usize) -> Option<usize> {
    let mut level = 0;
    for (k, b) in c.iter().enumerate().skip(open) {
        match b {
            b'(' => {
                level += 1;
                if level > depth {
                    return None;
                }
            }
            b')' => {
                level -= 1;
                if level == 0 {
                    return Some(k + 1);
                }
            }
            _ => {}
        }
    }
    None
}

/// `(?:\\\r?\n|[^\r\n])*` from `i`: a line with its `\` continuations.
fn logical_line_end(s: &[u8], mut i: usize) -> usize {
    while i < s.len() && !is_eol(s[i]) {
        if s[i] == b'\\' && at(s, i + 1) == b'\n' {
            i += 2;
        } else if s[i] == b'\\' && at(s, i + 1) == b'\r' && at(s, i + 2) == b'\n' {
            i += 3;
        } else {
            i += 1;
        }
    }
    i
}

fn block_comment_end(s: &[u8], i: usize) -> usize {
    memmem::find(&s[i + 2..], b"*/").map_or(s.len(), |p| i + 2 + p + 2)
}

/// The end of the string or char literal at `i`. The char-literal boundary leaves digit
/// separators (`1'000`) alone.
fn literal_end(s: &[u8], i: usize) -> Option<usize> {
    let (quote, mut j) = if s[i] == b'"' {
        (b'"', i + 1)
    } else {
        if i > 0 && is_word(s[i - 1]) {
            return None;
        }
        let q = match s[i] {
            b'\'' => i,
            b'L' | b'u' | b'U' if at(s, i + 1) == b'\'' => i + 1,
            b'u' if at(s, i + 1) == b'8' && at(s, i + 2) == b'\'' => i + 2,
            _ => return None,
        };
        (b'\'', q + 1)
    };
    while j < s.len() {
        if s[j] == b'\\' {
            if j + 1 >= s.len() {
                return None;
            }
            j += 2;
        } else if s[j] == quote {
            return Some(j + 1);
        } else {
            j += 1;
        }
    }
    Some(s.len())
}

/// `s` with comments as spaces and string/char literals as `literal`.
fn mask_literals(s: &[u8], literal: u8) -> Vec<u8> {
    let mut out = s.to_vec();
    let mut i = 0;
    while i < s.len() {
        if s[i] == b'/' && matches!(at(s, i + 1), b'/' | b'*') {
            let end = if s[i + 1] == b'/' { logical_line_end(s, i) } else { block_comment_end(s, i) };
            fill(&mut out, i, end, b' ');
            i = end;
        } else if let Some(end) = literal_end(s, i) {
            fill(&mut out, i, end, literal);
            i = end;
        } else {
            i += 1;
        }
    }
    out
}

/// Overwrite preprocessor directives, with their continuation lines, with `byte`.
fn mask_directives(s: &mut [u8], byte: u8) {
    let mut i = 0;
    while i < s.len() {
        let mut j = i;
        while matches!(at(s, j), b' ' | b'\t') {
            j += 1;
        }
        if at(s, j) == b'#' {
            let end = logical_line_end(s, j);
            fill(s, i, end, byte);
            i = end;
        }
        while i < s.len() && !is_eol(s[i]) {
            i += 1;
        }
        i += 1;
    }
}

/// The code the macro scans match on, at the source's offsets: comments and directives are
/// spaces, and a literal is a token of `\x01`s, so a run of macros never reaches across one.
fn mask_code(s: &[u8]) -> Vec<u8> {
    let mut code = mask_literals(s, 1);
    mask_directives(&mut code, b' ');
    code
}

/// The last source masked. The passes run one after another on the same text, and one that
/// blanks hands the next a mask blanked the same way.
#[derive(Default)]
struct Masked {
    source: Vec<u8>,
    code: Vec<u8>,
}

impl Masked {
    fn take(&mut self, source: &[u8]) -> Vec<u8> {
        if self.source == source {
            std::mem::take(&mut self.code)
        } else {
            mask_code(source)
        }
    }

    fn finish(&mut self, source: &mut [u8], spans: &[(usize, usize)], mut code: Vec<u8>) {
        for (from, to) in spans {
            fill(source, *from, *to, b' ');
            fill(&mut code, *from, *to, b' ');
        }
        self.source.clear();
        self.source.extend_from_slice(source);
        self.code = code;
    }
}

/// A span for each macro of the run at `[start, end)`, leaving what lies between them.
fn push_run_tokens(spans: &mut Vec<(usize, usize)>, c: &[u8], start: usize, end: usize) {
    let run = &c[start..end];
    let mut i = 0;
    while i < run.len() {
        let Some(mut j) = caps_end(run, i) else {
            i += 1;
            continue;
        };
        let k = skip_space(run, j);
        if at(run, k) == b'(' {
            if let Some(e) = balanced(run, k, 3) {
                j = e;
            }
        }
        spans.push((start + i, start + j));
        i = j;
    }
}

/// Hide raw literals from the scans: neither macro-shaped text in a body nor its `)delim"`
/// closer is code. Returns the masked text and the spans to restore.
fn mask_raw_strings(s: &[u8]) -> Option<MaskedRawStrings> {
    memmem::find(s, b"R\"")?;
    let mut out = s.to_vec();
    let mut spans = Vec::new();
    let mut i = 0;
    while i < s.len() {
        if s[i] == b'/' && at(s, i + 1) == b'/' {
            while i < s.len() && !is_eol(s[i]) {
                i += 1;
            }
        } else if s[i] == b'/' && at(s, i + 1) == b'*' {
            i = block_comment_end(s, i);
        } else if let Some((body, delim)) = raw_opener(s, i) {
            let closer = [b")", delim, b"\""].concat();
            // An unterminated raw literal owns the rest of the file too.
            let end = memmem::find(&s[body..], &closer).map_or(s.len(), |p| body + p + closer.len());
            spans.push((i, end));
            fill(&mut out, i, end, 0);
            i = end;
        } else if let Some(end) = literal_end(s, i) {
            i = end;
        } else {
            i += 1;
        }
    }
    (!spans.is_empty()).then_some((out, spans))
}

/// `\b(?:u8|[LuU])?R"delim(` at `i`: where the body starts, and the delimiter.
fn raw_opener(s: &[u8], i: usize) -> Option<(usize, &[u8])> {
    if i > 0 && is_word(s[i - 1]) {
        return None;
    }
    let r = match s[i] {
        b'R' => i,
        b'L' | b'u' | b'U' if at(s, i + 1) == b'R' => i + 1,
        b'u' if at(s, i + 1) == b'8' && at(s, i + 2) == b'R' => i + 2,
        _ => return None,
    };
    if at(s, r + 1) != b'"' {
        return None;
    }
    let delim = r + 2;
    let mut j = delim;
    while j < s.len() && j - delim <= 16 {
        match s[j] {
            b'(' => return Some((j + 1, &s[delim..j])),
            b')' | b'\\' => return None,
            b if is_space(b) => return None,
            _ => j += 1,
        }
    }
    None
}

fn restore_raw(masked: &mut [u8], original: &[u8], spans: &[(usize, usize)]) {
    for (from, to) in spans {
        for k in *from..*to {
            if masked[k] == 0 {
                masked[k] = original[k];
            }
        }
    }
}

/// Run `pass` with raw literals masked, for the blankers that scan the raw source.
fn with_raw_masked(s: &mut Vec<u8>, pass: impl FnOnce(&mut Vec<u8>)) {
    match mask_raw_strings(s) {
        Some((mut masked, spans)) => {
            pass(&mut masked);
            restore_raw(&mut masked, s, &spans);
            *s = masked;
        }
        None => pass(s),
    }
}

/// `class FMT_API x {`, `struct ALIGN_AS(64U) y : z`: blank the macros between the keyword and
/// the name, only where a base clause or body follows.
fn blank_export(s: &mut [u8], masked: &mut Masked) {
    if !EXPORT_PRE.is_match(s) {
        return;
    }
    let code = masked.take(s);
    let mut spans = Vec::new();
    let mut pos = 0;
    while let Some(m) = CLASS_KEYWORD.find_at(&code, pos) {
        let Some((run_end, end)) = class_head(&code, m.end()) else {
            pos = m.start() + 1;
            continue;
        };
        pos = end;
        let mut i = skip_space(&code, end);
        if at(&code, i) == b'<' {
            let Some(e) = skip_angle_args(&code, i) else { continue };
            i = skip_space(&code, e);
        }
        if code[i..].starts_with(b"final") && !is_word(at(&code, i + 5)) {
            i = skip_space(&code, i + 5);
        }
        if at(&code, i) != b'{' && !(at(&code, i) == b':' && at(&code, i + 1) != b':') {
            continue;
        }
        push_run_tokens(&mut spans, &code, m.end(), run_end);
    }
    masked.finish(s, &spans, code);
}

/// The macro run at `start` and the qualified name after it: `(run end, name end)`.
fn class_head(c: &[u8], start: usize) -> Option<(usize, usize)> {
    let mut ends = Vec::new();
    let mut i = start;
    while let Some(j) = caps_end(c, i).filter(|j| *j - i >= 2) {
        let k = skip_space(c, j);
        let with_args = (at(c, k) == b'(').then(|| balanced(c, k, 2)).flatten().filter(|e| is_space(at(c, *e)));
        i = if let Some(e) = with_args {
            skip_space(c, e)
        } else if k > j {
            k
        } else {
            break;
        };
        ends.push(i);
    }
    ends.into_iter().rev().find_map(|p| {
        let name = ident(c, p)?;
        if &c[p..name] == b"final" {
            return None;
        }
        let mut end = name;
        loop {
            let colons = skip_space(c, end);
            if !c[colons..].starts_with(b"::") {
                return Some((p, end));
            }
            match ident(c, skip_space(c, colons + 2)) {
                Some(e) => end = e,
                None => return Some((p, end)),
            }
        }
    })
}

/// Just past the template argument list opening at `open`, or `None` when it doesn't close
/// before a `;`, `{` or `}`: then it was a comparison.
fn skip_angle_args(c: &[u8], open: usize) -> Option<usize> {
    let (mut angles, mut parens) = (0i32, 0i32);
    for i in open..c.len().min(open + 4000) {
        match c[i] {
            b'(' => parens += 1,
            b')' => {
                parens -= 1;
                if parens < 0 {
                    return None;
                }
            }
            b';' | b'{' | b'}' => return None,
            b'<' if parens == 0 => angles += 1,
            b'>' if parens == 0 && c[i - 1] != b'-' => {
                angles -= 1;
                if angles == 0 {
                    return Some(i + 1);
                }
            }
            _ => {}
        }
    }
    None
}

/// Blank group 1 of each match of `re`; what follows it is only a lookahead.
fn blank_group(s: &mut [u8], re: &Regex) {
    let mut spans = Vec::new();
    let mut pos = 0;
    while let Some(group) = re.captures_at(s, pos).and_then(|c| c.get(1)) {
        spans.push((group.start(), group.end()));
        pos = group.end();
    }
    for (from, to) in spans {
        fill(s, from, to, b' ');
    }
}

/// `const Descriptor* PROTOBUF_NONNULL descriptor()`: an attribute between a pointer and the
/// name it declares.
fn blank_pointer_annotations(s: &mut [u8], masked: &mut Masked) {
    if !POINTER_PRE.is_match(s) {
        return;
    }
    let code = masked.take(s);
    let mut spans = Vec::new();
    let mut pos = 0;
    while pos < code.len() {
        if !matches!(code[pos], b'*' | b'&') {
            pos += 1;
            continue;
        }
        let run = skip_space(&code, pos + 1);
        let Some(end) = pointer_run(&code, run) else {
            pos += 1;
            continue;
        };
        let sign = code[pos];
        let idx = pos;
        pos = end;
        // After `&`, only a name: `flags & FOO_BIT\n  && ok` is an expression.
        if sign == b'&' && code[end] == b'*' {
            continue;
        }
        // A `*` after a closing bracket or a number multiplies: `sizeof(int) * CHAR_BIT * 3`.
        if follows_value(&code[idx.saturating_sub(40)..idx]) {
            continue;
        }
        push_run_tokens(&mut spans, &code, run, end);
    }
    masked.finish(s, &spans, code);
}

fn pointer_run(c: &[u8], start: usize) -> Option<usize> {
    let mut ends = Vec::new();
    let mut i = start;
    while let Some(j) = underscore_macro(c, i) {
        let k = skip_space(c, j);
        if k > j {
            ends.push(k);
            i = k;
        } else if at(c, j) == b'*' {
            ends.push(j);
            break;
        } else {
            break;
        }
    }
    ends.into_iter().rev().find(|p| {
        let b = at(c, *p);
        b == b'*' || c[*p..].starts_with(b"::") || b == b'~' || ident(c, *p).is_some_and(|e| !OPERATOR_WORDS.contains(&&c[*p..e]))
    })
}

/// `(?:[)\]]|\b\d[\w.]*)\s*$`
fn follows_value(before: &[u8]) -> bool {
    let mut e = before.len();
    while e > 0 && is_space(before[e - 1]) {
        e -= 1;
    }
    if e == 0 {
        return false;
    }
    if matches!(before[e - 1], b')' | b']') {
        return true;
    }
    let mut p = e;
    while p > 0 && (is_word(before[p - 1]) || before[p - 1] == b'.') {
        p -= 1;
        if before[p].is_ascii_digit() && (p == 0 || !is_word(before[p - 1])) {
            return true;
        }
    }
    false
}

/// `void Wait() LOCKS_EXCLUDED(mu_) {`, `~Any() PROTOBUF_FINAL;`: attributes after a parameter
/// list. Only what may end a declarator can follow them.
fn blank_trailing_attributes(s: &mut [u8], masked: &mut Masked) {
    if !TRAILING_PRE.is_match(s) {
        return;
    }
    let code = masked.take(s);
    let mut spans = Vec::new();
    let mut pos = 0;
    while let Some(close) = memchr::memchr(b')', &code[pos..]).map(|p| pos + p) {
        let macro_start = skip_space(&code, after_qualifiers(&code, close + 1));
        if caps_token(&code, macro_start, 3).is_none() {
            pos = close + 1;
            continue;
        }
        pos = macro_start;
        if BLANK_LINE.is_match(&code[close..macro_start]) {
            continue;
        }
        let end = macro_run(&code, macro_start, |c, i| caps_token(c, i, 3));
        if !ends_declarator(&code, end) || !closes_parameter_list(&code, close) {
            continue;
        }
        push_run_tokens(&mut spans, &code, macro_start, end);
        pos = end;
    }
    masked.finish(s, &spans, code);
}

fn after_qualifiers(c: &[u8], mut i: usize) -> usize {
    const QUALIFIERS: &[&[u8]] = &[b"const", b"volatile", b"noexcept", b"override", b"final", b"&&", b"&"];
    loop {
        let k = skip_space(c, i);
        match QUALIFIERS.iter().find(|q| c[k..].starts_with(q) && !is_word(at(c, k + q.len()))) {
            Some(q) => i = k + q.len(),
            None => return i,
        }
    }
}

/// The macros at `start`, each with optional arguments, and the space after them.
fn macro_run(c: &[u8], start: usize, token: impl Fn(&[u8], usize) -> Option<usize>) -> usize {
    let mut i = start;
    while let Some(mut j) = token(c, i) {
        let k = skip_space(c, j);
        if at(c, k) == b'(' {
            if let Some(e) = balanced(c, k, 3) {
                j = e;
            }
        }
        i = skip_space(c, j);
    }
    i
}

fn ends_declarator(c: &[u8], i: usize) -> bool {
    match at(c, i) {
        b'{' | b';' => true,
        b'=' => at(c, i + 1) != b'=',
        b':' => at(c, i + 1) != b':',
        b'-' => at(c, i + 1) == b'>',
        _ => [&b"override"[..], b"final", b"const", b"noexcept"].iter().any(|k| c[i..].starts_with(k) && !is_word(at(c, i + k.len()))),
    }
}

/// True when the `)` at `close` closes a function declarator's parameter list.
fn closes_parameter_list(c: &[u8], close: usize) -> bool {
    let mut depth = 0;
    let mut open = None;
    for i in (close.saturating_sub(3999)..=close).rev() {
        match c[i] {
            b')' => depth += 1,
            b'(' => {
                depth -= 1;
                if depth == 0 {
                    open = Some(i);
                    break;
                }
            }
            b';' | b'{' | b'}' => return false,
            _ => {}
        }
    }
    let Some(open) = open else { return false };
    let before = &c[open.saturating_sub(200)..open];
    if OPERATOR_TAIL.is_match(before) {
        return true;
    }
    NAME_TAIL.captures(before).and_then(|m| m.get(1)).is_some_and(|name| {
        let name = name.as_bytes();
        !PAREN_KEYWORDS.contains(&name) && caps_end(name, 0) != Some(name.len())
    })
}

/// `port::CondVar cv GUARDED_BY(mu);`: an attribute after the name a declaration declares.
fn blank_declarator_attributes(s: &mut [u8], masked: &mut Masked) {
    if !DECLARATOR_PRE.is_match(s) {
        return;
    }
    let code = masked.take(s);
    let mut spans = Vec::new();
    let mut pos = 0;
    while pos < code.len() {
        if !code[pos].is_ascii_uppercase() || (pos > 0 && (is_word(code[pos - 1]) || code[pos - 1] == b'$')) {
            pos += 1;
            continue;
        }
        let macro_start = pos;
        let Some(first) = underscore_macro(&code, pos).filter(|e| !is_word(at(&code, *e))) else {
            pos = word_end(&code, pos);
            continue;
        };
        pos = first;
        let mut last = macro_start;
        while last > 0 && is_space(code[last - 1]) {
            last -= 1;
        }
        // Whitespace before the macro, and before that a name or `]`.
        if last == macro_start || last == 0 || !(is_word(code[last - 1]) || code[last - 1] == b']') {
            continue;
        }
        let last = last - 1;
        let end = macro_run(&code, macro_start, |c, i| underscore_macro(c, i).filter(|e| !is_word(at(c, *e))));
        let follows = matches!(at(&code, end), b';' | b',' | b')') || (at(&code, end) == b'=' && at(&code, end + 1) != b'=');
        if !follows || (code[last] != b']' && !follows_declared_name(&code, last)) {
            continue;
        }
        push_run_tokens(&mut spans, &code, macro_start, end);
        pos = end;
    }
    masked.finish(s, &spans, code);
}

/// True when the identifier ending at `last` is a name a declaration declares: it follows a
/// type name, a type keyword, `*`, `&`, `>`, or `const` after a pointer.
fn follows_declared_name(c: &[u8], last: usize) -> bool {
    let is_keyword = |w: &[u8]| TYPE_KEYWORDS.contains(&w) || PAREN_KEYWORDS.contains(&w) || OTHER_KEYWORDS.contains(&w);
    let word_start = |mut i: usize| {
        while i > 0 && is_word(c[i - 1]) {
            i -= 1;
        }
        i
    };
    let back = |mut i: isize| {
        while i >= 0 && is_space(c[i as usize]) {
            i -= 1;
        }
        i
    };
    let start = word_start(last);
    let name = &c[start..=last];
    if ident(name, 0) != Some(name.len()) || !name.iter().any(u8::is_ascii_lowercase) || is_keyword(name) {
        return false;
    }
    let i = back(start as isize - 1);
    if i < 0 {
        return false;
    }
    let i = i as usize;
    match c[i] {
        b'*' | b'&' => return true,
        b'>' => return i == 0 || c[i - 1] != b'-',
        b if !is_word(b) => return false,
        _ => {}
    }
    let j = word_start(i);
    let before = &c[j..=i];
    if before == b"const" || before == b"volatile" {
        let k = back(j as isize - 1);
        return k >= 0 && matches!(c[k as usize], b'*' | b'&');
    }
    TYPE_KEYWORDS.contains(&before) || (ident(before, 0) == Some(before.len()) && !is_keyword(before))
}

/// Just past the `)` balancing the `(` at `open`, skipping string and char literals.
fn balance_past_literals(s: &[u8], open: usize) -> Option<usize> {
    let mut depth = 0;
    let mut i = open;
    while i < s.len() {
        match s[i] {
            q @ (b'"' | b'\'') => {
                i += 1;
                while i < s.len() && s[i] != q {
                    if s[i] == b'\\' {
                        i += 1;
                    }
                    i += 1;
                }
            }
            b'(' => depth += 1,
            b')' => {
                depth -= 1;
                if depth == 0 {
                    return Some(i + 1);
                }
            }
            _ => {}
        }
        i += 1;
    }
    None
}

/// `UPROPERTY(..)` on its own line above a member: markup is followed by what it decorates,
/// where a statement call is followed by `;` and an expression by an operator.
fn blank_annotation_calls(s: &mut Vec<u8>) {
    if !ANNOTATION_CALL.is_match(s) {
        return;
    }
    with_raw_masked(s, |s| {
        let mut spans = Vec::new();
        let mut pos = 0;
        while let Some(m) = ANNOTATION_CALL.captures_at(s, pos) {
            let whole = m.get(0).unwrap();
            pos = whole.end();
            let Some(end) = balance_past_literals(s, whole.end() - 1) else { continue };
            let after = at(s, skip_space(s, end));
            if after.is_ascii_alphabetic() || matches!(after, b'_' | b'~' | b'#') {
                spans.push((m.get(1).unwrap().start(), end));
                pos = end;
            }
        }
        for (from, to) in spans {
            fill(s, from, to, b' ');
        }
    });
}

/// `UMETA(DisplayName="…")`, `UE_DEPRECATED(5.5, "…")` mid-line, by their Unreal-only names.
fn blank_inline_annotations(s: &mut Vec<u8>) {
    if !INLINE_ANNOTATION_PRE.is_match(s) {
        return;
    }
    with_raw_masked(s, |s| {
        let mut spans = Vec::new();
        let mut pos = 0;
        while let Some(m) = INLINE_ANNOTATION.find_at(s, pos) {
            pos = m.end();
            if let Some(end) = balance_past_literals(s, m.end() - 1) {
                spans.push((m.start(), end));
                pos = end;
            }
        }
        for (from, to) in spans {
            fill(s, from, to, b' ');
        }
    });
}

/// `GTEST_API_ std::string JoinAsTuple(..)`, `[[nodiscard]] PROTOBUF_NDEBUG_INLINE Ptr<T> Make(..)`:
/// a macro opening a declaration. It must be followed by what can't follow a type name.
fn blank_leading_attributes(s: &mut [u8], masked: &mut Masked) {
    if !LEADING_PRE.is_match(s) {
        return;
    }
    let code = masked.take(s);
    let mut spans = Vec::new();
    let mut pos = 0;
    while pos < code.len() {
        let line_start = pos == 0 || is_eol(code[pos - 1]);
        let brackets = code[pos..].starts_with(b"]]");
        let mut found = None;
        if line_start {
            found = leading_run(&code, pos);
        }
        if found.is_none() && brackets {
            found = leading_run(&code, pos + 2);
        }
        if let Some((start, end)) = found {
            push_run_tokens(&mut spans, &code, start, end);
            pos = end;
        } else {
            pos += 1;
        }
    }
    masked.finish(s, &spans, code);
}

fn leading_run(c: &[u8], indent: usize) -> Option<(usize, usize)> {
    let mut start = indent;
    while matches!(at(c, start), b' ' | b'\t') {
        start += 1;
    }
    let mut ends = Vec::new();
    let mut i = start;
    while let Some(j) = underscore_macro(c, i) {
        let k = skip_space(c, j);
        if k == j {
            break;
        }
        ends.push(k);
        i = k;
    }
    ends.into_iter().rev().find(|p| starts_declaration(c, *p)).map(|p| (start, p))
}

fn starts_declaration(c: &[u8], p: usize) -> bool {
    if ident(c, p).is_some_and(|e| DECL_KEYWORDS.contains(&&c[p..e])) || c[p..].starts_with(b"[[") || out_of_line_constructor(c, p) {
        return true;
    }
    // `(?:(?:const|volatile)\s+)*` and then a type that is none of these words.
    let mut q = p;
    while let Some(e) = ident(c, q).filter(|e| matches!(&c[q..*e], b"const" | b"volatile") && is_space(at(c, *e))) {
        q = skip_space(c, e);
    }
    let name = if c[q..].starts_with(b"::") { skip_space(c, q + 2) } else { q };
    let blocked = ident(c, name).is_none_or(|e| matches!(&c[name..e], b"const" | b"volatile" | b"operator" | b"return"));
    !blocked && TYPE_THEN_NAME.is_match(&c[q..])
}

/// `Any::Impl_::Impl_(`, `Any::~Any(`
fn out_of_line_constructor(c: &[u8], mut i: usize) -> bool {
    let mut prev: Option<&[u8]> = None;
    loop {
        let Some(e) = ident(c, i) else { return false };
        let name = &c[i..e];
        let next = skip_space(c, e);
        if !c[next..].starts_with(b"::") {
            return prev == Some(name) && at(c, next) == b'(';
        }
        let k = skip_space(c, next + 2);
        if at(c, k) == b'~' {
            let Some(e) = ident(c, k + 1) else { return false };
            return &c[k + 1..e] == name && at(c, skip_space(c, e)) == b'(';
        }
        prev = Some(name);
        i = k;
    }
}

fn trimmed(line: &[u8]) -> &[u8] {
    line.trim_ascii()
}

fn line_ranges(s: &[u8]) -> Vec<(usize, usize)> {
    let mut lines = Vec::new();
    let mut start = 0;
    for p in memchr::memchr_iter(b'\n', s) {
        lines.push((start, p));
        start = p + 1;
    }
    lines.push((start, s.len()));
    lines
}

/// `FMT_BEGIN_NAMESPACE`, `Q_OBJECT`: a macro alone on its line, where the line above continues
/// nothing and the code below starts a declaration.
fn blank_lone_macro_lines(s: &mut [u8]) {
    if !LONE_PRE.is_match(s) {
        return;
    }
    let lines = line_ranges(s);
    for i in 0..lines.len() {
        let (from, to) = lines[i];
        let Some(group) = LONE_LINE.captures(&s[from..to]).and_then(|c| c.get(1)) else { continue };
        if !group.as_bytes().contains(&b'_') {
            continue;
        }
        let span = (from + group.start(), from + group.end());
        let mut prev = i;
        let mut prev_text: &[u8] = b"";
        while prev > 0 {
            prev -= 1;
            prev_text = trimmed(&s[lines[prev].0..lines[prev].1]);
            if !prev_text.is_empty() {
                break;
            }
        }
        // Judged on the line's code: a comment ends in whatever it likes but continues nothing.
        let prev_code = if prev_text.starts_with(b"//") || prev_text.starts_with(b"/*") || prev_text.starts_with(b"*") {
            Vec::new()
        } else {
            trimmed(&COMMENT_IN_LINE.replace_all(prev_text, &b""[..])).to_vec()
        };
        let directive_ends = prev_code.starts_with(b"#") && !prev_code.ends_with(b"\\");
        let continues = prev_code.last().is_some_and(|b| b"=+-*/%&|^<>?:,(\\".contains(b));
        if !directive_ends && !TEMPLATE_HEAD.is_match(&prev_code) && continues {
            continue;
        }
        if first_code_after(s, &lines[i + 1..]).is_some_and(|b| !(b.is_ascii_alphabetic() || matches!(b, b'_' | b'#' | b'{' | b'}' | b'~'))) {
            continue;
        }
        fill(s, span.0, span.1, b' ');
    }
}

/// The first byte of code in `lines`, past blank lines and comments.
fn first_code_after(s: &[u8], lines: &[(usize, usize)]) -> Option<u8> {
    let mut in_block = false;
    for (from, to) in lines {
        let mut text = trimmed(&s[*from..*to]);
        loop {
            if in_block {
                match memmem::find(text, b"*/") {
                    Some(end) => {
                        text = trimmed(&text[end + 2..]);
                        in_block = false;
                    }
                    None => {
                        text = b"";
                        break;
                    }
                }
            }
            if text.starts_with(b"//") {
                text = b"";
            } else if text.starts_with(b"/*") {
                in_block = true;
                text = &text[2..];
                continue;
            }
            break;
        }
        if let Some(b) = text.first() {
            return Some(*b);
        }
    }
    None
}

/// Copy directive lines back: a blank that hit `#define FMT_API ..` leaves a nameless define,
/// a parse error, and blanking inside a directive is never useful.
fn restore_directive_lines(original: &[u8], blanked: &mut [u8]) {
    if original == blanked || !original.contains(&b'#') {
        return;
    }
    let mut continuation = false;
    for (from, to) in line_ranges(original) {
        let line = &original[from..to];
        let mut indent = 0;
        while matches!(at(line, indent), b' ' | b'\t') {
            indent += 1;
        }
        let is_directive = continuation || at(line, indent) == b'#';
        if is_directive {
            blanked[from..to].copy_from_slice(line);
        }
        continuation = is_directive && line.trim_ascii_end().ends_with(b"\\");
    }
}

/// MSVC's COM alias `interface X : Y {` at type-definition sites becomes `struct`.
fn normalize_com_interfaces(s: &mut [u8]) {
    if memmem::find(s, b"interface").is_none() {
        return;
    }
    let mut code = mask_literals(s, b' ');
    let has_alias = COM_ALIAS.is_match(&code);
    mask_directives(&mut code, 0);
    let mut offsets = Vec::new();
    let mut pos = 0;
    while let Some(m) = COM_DECLARATION.captures_at(&code, pos) {
        let whole = m.get(0).unwrap();
        pos = whole.end();
        let body_end = memchr::memchr(b'}', &code[pos..]).map_or(code.len(), |p| pos + p);
        if !has_alias && m.get(1).is_none() && !VIRTUAL.is_match(&code[pos..body_end]) {
            continue;
        }
        offsets.push(whole.start() + memmem::find(whole.as_bytes(), b"interface").unwrap());
        // The opening brace may start a nested declaration.
        pos -= 1;
    }
    for offset in offsets {
        s[offset..offset + 9].copy_from_slice(b"struct   ");
    }
}

/// `JSON_PRIVATE_UNLESS_TESTED:`, `Q_SIGNALS:`: an access specifier spelled as a macro becomes
/// the keyword it stands for, padded to the same length.
fn rewrite_access_macros(s: &mut [u8]) {
    let mut edits = Vec::new();
    for m in ACCESS_MACRO.captures_iter(s) {
        let (name, gap) = (m.get(2).unwrap(), m.get(3).unwrap());
        let has = |word: &[u8]| memmem::find(name.as_bytes(), word).is_some();
        let keyword: &[u8] = if has(b"PRIVATE") {
            b"private"
        } else if has(b"PROTECTED") {
            b"protected"
        } else if has(b"PUBLIC") || matches!(name.as_bytes(), b"Q_SIGNALS" | b"Q_SLOTS") {
            b"public"
        } else {
            continue;
        };
        if keyword.len() <= gap.end() - name.start() {
            edits.push((name.start(), gap.end(), keyword));
        }
    }
    for (from, to, keyword) in edits {
        s[from..from + keyword.len()].copy_from_slice(keyword);
        s[from + keyword.len()..to].fill(b' ');
    }
}

/// The code of a line: comments removed the way upstream does it, trimmed.
fn line_code(line: &[u8]) -> Vec<u8> {
    if !line.contains(&b'/') {
        return trimmed(line).to_vec();
    }
    let no_line = LINE_COMMENT_END.replace(line, &b""[..]);
    trimmed(&BLOCK_COMMENT_INLINE.replace_all(&no_line, &b""[..])).to_vec()
}

/// A preprocessor conditional in the middle of a declaration or expression is an error to the
/// grammar. Where the code before the `#if` has not ended a statement, its directives are
/// blanked and so is every branch after the first: one consistent configuration.
fn flatten_mid_statement_conditionals(s: &mut [u8]) {
    if !s.contains(&b'#') {
        return;
    }
    fn blank_line(s: &mut [u8], (from, to): (usize, usize)) {
        for b in &mut s[from..to] {
            if *b != b'\r' {
                *b = b' ';
            }
        }
    }
    fn continues(line: &[u8]) -> bool {
        line.trim_ascii_end().ends_with(b"\\") || line.iter().rposition(|b| !is_space(*b)).is_some_and(|p| line[p] == b'\\')
    }
    let mut last_code: Vec<u8> = Vec::new();
    let mut in_block_comment = false;
    let mut region = 0usize;
    let mut skip_from = 0usize;
    let mut continued = false;
    for range in line_ranges(s) {
        let line = &s[range.0..range.1];
        if continued {
            continued = continues(line);
            continue;
        }
        let directive = DIRECTIVE_NAME.captures(line).and_then(|c| c.get(1)).map(|d| d.as_bytes().to_vec());
        if let (Some(d), false) = (&directive, in_block_comment) {
            // A directive continued with `\` spans lines; its body is not code.
            if continues(line) {
                continued = true;
                if region == 0 {
                    continue;
                }
            }
            let opens = d.starts_with(b"if");
            if region == 0 {
                // Not after a statement macro written without its `;` either.
                let mid = opens
                    && !last_code.is_empty()
                    && !ENDS_STATEMENT.is_match(&last_code)
                    && !ACCESS_LABEL.is_match(&last_code)
                    && !STATEMENT_MACRO.is_match(&last_code);
                if mid {
                    region = 1;
                    blank_line(s, range);
                }
                continue;
            }
            if opens {
                region += 1;
            } else if (d == b"else" || d == b"elif") && skip_from == 0 {
                skip_from = region;
            } else if d == b"endif" {
                if skip_from == region {
                    skip_from = 0;
                }
                region -= 1;
            }
            if CONDITIONAL.is_match(d) {
                blank_line(s, range);
            }
            continue;
        }
        if region > 0 && skip_from > 0 {
            if !trimmed(line).is_empty() {
                blank_line(s, range);
            }
            continue;
        }
        // Track block comments so a `#` or `;` inside one is not read as code.
        if in_block_comment {
            if memmem::find(line, b"*/").is_some() {
                in_block_comment = false;
            }
            continue;
        }
        let code = line_code(line);
        let no_line_comment = LINE_COMMENT_END.replace(line, &b""[..]);
        let visible = no_line_comment.split(|b| *b == b'\r').next().unwrap_or(b"");
        let opens_comment = memmem::rfind(visible, b"/*").is_some_and(|p| memmem::find(&visible[p + 2..], b"*/").is_none());
        if opens_comment {
            in_block_comment = true;
            let before = memmem::find(&code, b"/*").map_or(&code[..], |p| &code[..p]);
            let before = trimmed(before);
            if !before.is_empty() {
                last_code = before.to_vec();
            }
            continue;
        }
        if !code.is_empty() {
            last_code = code;
        }
    }
}

/// `typedef NS_ENUM(NSInteger, Kind) {` becomes `        enum           Kind  {`.
fn rewrite_foundation_enums(s: &mut [u8], masked: &mut Masked) {
    if !FOUNDATION_PRE.is_match(s) {
        return;
    }
    let code = masked.take(s);
    for m in FOUNDATION_ENUM.captures_iter(&code) {
        if let Some(typedef) = m.get(1) {
            fill(s, typedef.start(), typedef.end(), b' ');
        }
        let name = m.get(3).unwrap();
        s[name.start()..name.start() + 4].copy_from_slice(b"enum");
        fill(s, name.start() + 4, name.end(), b' ');
        // `(`, the first argument and the `,`.
        let first = m.get(5).unwrap();
        fill(s, first.start() - 1, first.end() + 1, b' ');
        let close = m.get(8).unwrap().end();
        fill(s, close, close + 1, b' ');
    }
    masked.source.clear();
    masked.code = code;
}

fn blank_metal_attributes(s: &mut [u8]) {
    let spans: Vec<_> = METAL_ATTRIBUTE.find_iter(s).map(|m| m.range()).collect();
    for r in spans {
        s[r].fill(b' ');
    }
}

/// `__global__ void step(..)`, `step<<<grid, block>>>(args)`: the CUDA surface the C++ grammar
/// can't reconcile. A launch config is blanked only when its braces balance.
fn blank_cuda_constructs(s: &mut [u8]) {
    for re in [&*CUDA_LAUNCH_BOUNDS, &*CUDA_SPECIFIER] {
        let spans: Vec<_> = re.find_iter(s).map(|m| m.range()).collect();
        for r in spans {
            s[r].fill(b' ');
        }
    }
    let spans: Vec<_> = CUDA_LAUNCH_CONFIG.find_iter(s).filter(|m| braces_balance(m.as_bytes())).map(|m| m.range()).collect();
    for r in spans {
        for b in &mut s[r] {
            if *b != b'\n' {
                *b = b' ';
            }
        }
    }
}

fn braces_balance(text: &[u8]) -> bool {
    let mut depth = 0i32;
    for b in text {
        match b {
            b'{' => depth += 1,
            b'}' => {
                depth -= 1;
                if depth < 0 {
                    return false;
                }
            }
            _ => {}
        }
    }
    depth == 0
}

fn looks_like_cuda(s: &[u8]) -> bool {
    [&b"__global__"[..], b"__device__", b"__constant__", b"cudaStream_t"].iter().any(|m| memmem::find(s, m).is_some())
}

/// The masked text and the byte ranges that were masked.
type MaskedRawStrings = (Vec<u8>, Vec<(usize, usize)>);

/// The text the C++ kernel and facts read for `path`, at the offsets of `text`.
pub fn pre_parse(text: &str, path: &str) -> String {
    let original = text.as_bytes();
    let raw = mask_raw_strings(original);
    let mut s = raw.as_ref().map_or_else(|| original.to_vec(), |(m, _)| m.clone());
    let mut masked = Masked::default();
    rewrite_foundation_enums(&mut s, &mut masked);
    rewrite_access_macros(&mut s);
    flatten_mid_statement_conditionals(&mut s);
    let flattened = s.clone();
    normalize_com_interfaces(&mut s);
    blank_export(&mut s, &mut masked);
    blank_group(&mut s, &INLINE_MACRO);
    blank_group(&mut s, &API_PREFIX);
    blank_inline_annotations(&mut s);
    blank_pointer_annotations(&mut s, &mut masked);
    blank_trailing_attributes(&mut s, &mut masked);
    blank_declarator_attributes(&mut s, &mut masked);
    blank_annotation_calls(&mut s);
    // After the api-prefix blank: `FMT_NORETURN FMT_API void f(` reduces to `MACRO Ret name(`.
    blank_group(&mut s, &C_LEADING);
    blank_leading_attributes(&mut s, &mut masked);
    blank_lone_macro_lines(&mut s);
    let lower = path.to_ascii_lowercase();
    if lower.ends_with(".metal") {
        blank_metal_attributes(&mut s);
    } else if lower.ends_with(".cu") || lower.ends_with(".cuh") || looks_like_cuda(&flattened) {
        blank_cuda_constructs(&mut s);
    }
    restore_directive_lines(&flattened, &mut s);
    if let Some((_, spans)) = &raw {
        restore_raw(&mut s, original, spans);
    }
    String::from_utf8(s).unwrap_or_else(|_| text.to_owned())
}

#[cfg(test)]
mod tests {
    use super::pre_parse;

    fn pp(src: &str) -> String {
        let out = pre_parse(src, "a.cpp");
        assert_eq!(out.len(), src.len(), "offsets move in {src:?}");
        out
    }

    #[test]
    fn blanks_the_macros_the_grammar_trips_on() {
        assert_eq!(pp("class FMT_API x : y {};"), "class         x : y {};");
        assert_eq!(pp("struct ALIGN_AS(64U) H {};"), "struct               H {};");
        assert_eq!(pp("FMT_BEGIN_NAMESPACE\nint x;\n"), "                   \nint x;\n");
        assert_eq!(pp("FMT_CONSTEXPR auto f() -> int;"), "              auto f() -> int;");
        assert_eq!(pp("void Wait() LOCKS_EXCLUDED(mu_) {}"), "void Wait()                     {}");
        assert_eq!(pp("Foo cv GUARDED_BY(mu);"), "Foo cv               ;");
        assert_eq!(pp("Any* PROTOBUF_NONNULL other();"), "Any*                  other();");
        assert_eq!(pp("GTEST_API_ std::string Join(int a);"), "           std::string Join(int a);");
        assert_eq!(pp("FOO_X Any::Any(int a) {}"), "      Any::Any(int a) {}");
        assert_eq!(pp("UPROPERTY(Edit)\nfloat x;"), "               \nfloat x;");
        assert_eq!(pp("JSON_PRIVATE_UNLESS_TESTED:\n"), "private                   :\n");
    }

    #[test]
    fn leaves_expressions_literals_and_directives_alone() {
        for src in [
            "class FOO x;",
            "int y = a * MAX_LEN + 1;",
            "int z = sizeof(int) * CHAR_BIT * 3;",
            "void f() { if (x) FOO_BAR; }",
            "#define FMT_API FMT_VISIBILITY(\"default\")\n",
            "int x =\n  SOME_CONST\n  | OTHER;\n",
            "auto s = R\"(FMT_BEGIN_NAMESPACE\n)\";\n",
        ] {
            assert_eq!(pp(src), src);
        }
    }

    #[test]
    fn keeps_one_branch_of_a_conditional_inside_a_declaration() {
        let src = "A::A() noexcept\n#ifdef X\n : a(1)\n#else\n : a(2)\n#endif\n{}\n";
        assert_eq!(pp(src), "A::A() noexcept\n        \n : a(1)\n     \n       \n      \n{}\n");
    }
}
