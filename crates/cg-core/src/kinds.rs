//! Enumerations shared across the engine. The numbering of `NodeKind` and `EdgeKind`
//! is the kernel wire format (design doc 16.2) and is verified at startup against
//! `cg_kernel::contract_info()`.

macro_rules! kind_enum {
    ($(#[$m:meta])* $name:ident { $($var:ident = $s:literal),+ $(,)? }) => {
        $(#[$m])*
        #[repr(u8)]
        #[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Debug)]
        pub enum $name { $($var),+ }
        impl $name {
            pub const ALL: &'static [$name] = &[$($name::$var),+];
            pub const NAMES: &'static [&'static str] = &[$($s),+];
            #[inline] pub fn as_str(self) -> &'static str { Self::NAMES[self as usize] }
            #[inline] pub fn from_u8(v: u8) -> Option<$name> { Self::ALL.get(v as usize).copied() }
            pub fn parse(s: &str) -> Option<$name> {
                Self::NAMES.iter().position(|n| *n == s).map(|i| Self::ALL[i])
            }
        }
        impl std::fmt::Display for $name {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result { f.write_str(self.as_str()) }
        }
    };
}

kind_enum!(
    /// 23 node kinds (design doc 0.1).
    NodeKind {
        File = "file", Module = "module", Class = "class", Struct = "struct",
        Interface = "interface", Trait = "trait", Protocol = "protocol", Function = "function",
        Method = "method", Property = "property", Field = "field", Variable = "variable",
        Constant = "constant", Enum = "enum", EnumMember = "enum_member", TypeAlias = "type_alias",
        Namespace = "namespace", Parameter = "parameter", Import = "import", Export = "export",
        Route = "route", Component = "component", Union = "union",
    }
);

kind_enum!(
    /// 13 edge kinds.
    EdgeKind {
        Contains = "contains", Calls = "calls", Imports = "imports", Exports = "exports",
        Extends = "extends", Implements = "implements", References = "references",
        TypeOf = "type_of", Returns = "returns", Instantiates = "instantiates",
        Overrides = "overrides", Decorates = "decorates", Navigates = "navigates",
    }
);

kind_enum!(
    /// Who produced an edge, as stored in the 2 high bits of `out_kind` (design doc 5.3).
    Provenance {
        None = "", TreeSitter = "tree-sitter", Scip = "scip", Heuristic = "heuristic",
    }
);

kind_enum!(
    Visibility {
        None = "", Public = "public", Private = "private", Protected = "protected", Internal = "internal",
    }
);

impl NodeKind {
    /// Class-like things a `calls` edge may target through construction, and the
    /// targets of `extends` / `implements` / `type_of`.
    #[inline]
    pub fn is_type(self) -> bool {
        matches!(
            self,
            NodeKind::Class
                | NodeKind::Struct
                | NodeKind::Interface
                | NodeKind::Trait
                | NodeKind::Protocol
                | NodeKind::Enum
                | NodeKind::TypeAlias
                | NodeKind::Union
        )
    }
    /// Things that can be called.
    #[inline]
    pub fn is_callable(self) -> bool {
        matches!(self, NodeKind::Function | NodeKind::Method | NodeKind::Component | NodeKind::Route)
    }
    /// Kinds that own members through `contains` edges.
    #[inline]
    pub fn is_container(self) -> bool {
        self.is_type() || matches!(self, NodeKind::Module | NodeKind::Namespace | NodeKind::File)
    }
    /// Kinds that are symbols a user searches for (everything but file/import/export/parameter).
    #[inline]
    pub fn is_definition(self) -> bool {
        !matches!(self, NodeKind::File | NodeKind::Import | NodeKind::Export | NodeKind::Parameter)
    }
}

impl EdgeKind {
    #[inline]
    pub fn code(self) -> u8 {
        self as u8
    }
}

/// Kind of an unresolved reference: an edge kind, or the internal-only `function_ref`
/// (a function passed as a value, wire code 200).
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum RefKind {
    Edge(EdgeKind),
    FunctionRef,
}

impl RefKind {
    pub const FUNCTION_REF_WIRE: u8 = 200;
    #[inline]
    pub fn from_wire(b: u8) -> Option<RefKind> {
        if b == Self::FUNCTION_REF_WIRE {
            Some(RefKind::FunctionRef)
        } else {
            EdgeKind::from_u8(b).map(RefKind::Edge)
        }
    }
    #[inline]
    pub fn to_wire(self) -> u8 {
        match self {
            RefKind::Edge(k) => k as u8,
            RefKind::FunctionRef => Self::FUNCTION_REF_WIRE,
        }
    }
    pub fn as_str(self) -> &'static str {
        match self {
            RefKind::Edge(k) => k.as_str(),
            RefKind::FunctionRef => "function_ref",
        }
    }
    #[inline]
    pub fn edge_kind(self) -> EdgeKind {
        match self {
            RefKind::Edge(k) => k,
            RefKind::FunctionRef => EdgeKind::References,
        }
    }
}

/// Languages. `Unknown` is 0 so zeroed records mean "no language". Walkers exist for
/// the MVP set (design doc 1.3) and C/C++, C#, PHP, Ruby and Swift; the rest are
/// detected so configuration stays forward compatible.
#[repr(u8)]
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Debug)]
pub enum Lang {
    Unknown = 0,
    TypeScript = 1,
    Tsx = 2,
    JavaScript = 3,
    Jsx = 4,
    Python = 5,
    Go = 6,
    Java = 7,
    Rust = 8,
    Kotlin = 9,
    C = 10,
    Cpp = 11,
    CSharp = 12,
    Php = 13,
    Ruby = 14,
    Swift = 15,
    Dart = 16,
    Scala = 17,
    Lua = 18,
    Luau = 19,
    R = 20,
    /// Detected by content in `.h` headers so they are not fed to the C walker; no walker.
    ObjC = 21,
    Vue = 22,
    Svelte = 23,
}

/// Languages whose symbols may reference one another by name (design doc 6.5: "same
/// language family"; cross-family edges only through framework rules).
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum LangFamily {
    Js,
    Python,
    Go,
    Jvm,
    Rust,
    C,
    CSharp,
    Php,
    Ruby,
    Swift,
    Dart,
    Lua,
    R,
    Unknown,
}

impl Lang {
    pub const ALL: &'static [Lang] = &[
        Lang::TypeScript,
        Lang::Tsx,
        Lang::JavaScript,
        Lang::Jsx,
        Lang::Python,
        Lang::Go,
        Lang::Java,
        Lang::Rust,
        Lang::Kotlin,
        Lang::C,
        Lang::Cpp,
        Lang::CSharp,
        Lang::Php,
        Lang::Ruby,
        Lang::Swift,
        Lang::Dart,
        Lang::Scala,
        Lang::Lua,
        Lang::Luau,
        Lang::R,
        Lang::ObjC,
        Lang::Vue,
        Lang::Svelte,
    ];

    /// Upstream `Language` string; also the kernel's language argument.
    pub fn as_str(self) -> &'static str {
        match self {
            Lang::Unknown => "unknown",
            Lang::TypeScript => "typescript",
            Lang::Tsx => "tsx",
            Lang::JavaScript => "javascript",
            Lang::Jsx => "jsx",
            Lang::Python => "python",
            Lang::Go => "go",
            Lang::Java => "java",
            Lang::Rust => "rust",
            Lang::Kotlin => "kotlin",
            Lang::C => "c",
            Lang::Cpp => "cpp",
            Lang::CSharp => "csharp",
            Lang::Php => "php",
            Lang::Ruby => "ruby",
            Lang::Swift => "swift",
            Lang::Dart => "dart",
            Lang::Scala => "scala",
            Lang::Lua => "lua",
            Lang::Luau => "luau",
            Lang::R => "r",
            Lang::ObjC => "objc",
            Lang::Vue => "vue",
            Lang::Svelte => "svelte",
        }
    }

    pub fn parse(s: &str) -> Lang {
        Lang::ALL.iter().copied().find(|l| l.as_str() == s).unwrap_or(Lang::Unknown)
    }

    #[inline]
    pub fn from_u8(v: u8) -> Lang {
        if v == 0 {
            return Lang::Unknown;
        }
        Lang::ALL.iter().copied().find(|l| *l as u8 == v).unwrap_or(Lang::Unknown)
    }

    /// Whether this build has a walker for the language (MVP set, design doc 1.3 + D2,
    /// plus C/C++, C#, PHP, Ruby, Swift, and Vue/Svelte SFC extraction).
    pub fn is_supported(self) -> bool {
        matches!(
            self,
            Lang::TypeScript
                | Lang::Tsx
                | Lang::JavaScript
                | Lang::Jsx
                | Lang::Vue
                | Lang::Svelte
                | Lang::Python
                | Lang::Go
                | Lang::Java
                | Lang::Rust
                | Lang::Kotlin
                | Lang::C
                | Lang::Cpp
                | Lang::CSharp
                | Lang::Php
                | Lang::Ruby
                | Lang::Swift
        )
    }

    pub fn family(self) -> LangFamily {
        match self {
            Lang::TypeScript | Lang::Tsx | Lang::JavaScript | Lang::Jsx | Lang::Vue | Lang::Svelte => LangFamily::Js,
            Lang::Python => LangFamily::Python,
            Lang::Go => LangFamily::Go,
            Lang::Java | Lang::Kotlin | Lang::Scala => LangFamily::Jvm,
            Lang::Rust => LangFamily::Rust,
            Lang::C | Lang::Cpp => LangFamily::C,
            Lang::CSharp => LangFamily::CSharp,
            Lang::Php => LangFamily::Php,
            Lang::Ruby => LangFamily::Ruby,
            Lang::Swift => LangFamily::Swift,
            Lang::Dart => LangFamily::Dart,
            Lang::Lua | Lang::Luau => LangFamily::Lua,
            Lang::R => LangFamily::R,
            Lang::Unknown | Lang::ObjC => LangFamily::Unknown,
        }
    }

    #[inline]
    pub fn is_js_family(self) -> bool {
        self.family() == LangFamily::Js
    }
    #[inline]
    pub fn is_typescript(self) -> bool {
        matches!(self, Lang::TypeScript | Lang::Tsx)
    }
}

impl std::fmt::Display for Lang {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wire_order_matches_design_doc() {
        assert_eq!(NodeKind::ALL.len(), 23);
        assert_eq!(EdgeKind::ALL.len(), 13);
        assert_eq!(NodeKind::File as u8, 0);
        assert_eq!(NodeKind::Union as u8, 22);
        assert_eq!(EdgeKind::Navigates as u8, 12);
        assert_eq!(NodeKind::parse("enum_member"), Some(NodeKind::EnumMember));
        assert_eq!(RefKind::from_wire(200), Some(RefKind::FunctionRef));
        assert_eq!(RefKind::from_wire(1), Some(RefKind::Edge(EdgeKind::Calls)));
        assert_eq!(RefKind::from_wire(99), None);
    }

    #[test]
    fn lang_roundtrip() {
        for &l in Lang::ALL {
            assert_eq!(Lang::parse(l.as_str()), l);
            assert_eq!(Lang::from_u8(l as u8), l);
        }
        assert_eq!(Lang::parse("klingon"), Lang::Unknown);
        assert_eq!(Lang::Kotlin.family(), Lang::Java.family());
    }
}
