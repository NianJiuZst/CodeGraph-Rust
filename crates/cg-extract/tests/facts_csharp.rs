//! C# facts: package, `using` directives, scopes and typed bindings.

use cg_core::Lang;
use cg_extract::*;

fn run(src: &str) -> Facts {
    let b = extract_bytes("A.cs", src.as_bytes(), 0, Lang::CSharp, &ExtractConfig::new("."));
    assert_eq!(b.flags & fileflags::PANICKED, 0, "{:?}", b.errors);
    b.facts
}

fn imp<'a>(f: &'a Facts, spec: &str) -> &'a ImportFact {
    f.imports.iter().find(|i| i.spec == spec).unwrap_or_else(|| panic!("no import {spec}: {:?}", f.imports))
}

fn bind<'a>(f: &'a Facts, name: &str) -> &'a BindingFact {
    f.bindings.iter().find(|b| b.name == name).unwrap_or_else(|| panic!("no binding {name}: {:?}", f.bindings))
}

fn bind_kind<'a>(f: &'a Facts, name: &str, kind: BindKind) -> &'a BindingFact {
    f.bindings.iter().find(|b| b.name == name && b.kind == kind).unwrap_or_else(|| panic!("no {kind:?} binding {name}: {:?}", f.bindings))
}

/// (decl_type, init_ctor) of the binding called `name`.
fn ty<'a>(f: &'a Facts, name: &str) -> (&'a str, &'a str) {
    let b = bind(f, name);
    (b.decl_type.as_str(), b.init_ctor.as_str())
}

fn scope_kind(f: &Facts, b: &BindingFact) -> ScopeKind {
    f.scopes[b.scope as usize].kind
}

fn packages(f: &Facts) -> Vec<(&str, u32, u32)> {
    f.bindings.iter().filter(|b| b.kind == BindKind::Package).map(|b| (b.name.as_str(), b.scope, b.line)).collect()
}

#[test]
fn block_namespace_is_the_package() {
    let f = run(r#"using System;

namespace Acme.Billing
{
    namespace Inner { class X { } }
    class Invoice { }
}

namespace Other.Ns { class Y { } }
"#);
    // only the first (outer) namespace of the file
    assert_eq!(packages(&f), vec![("Acme.Billing", 0, 3)]);
}

#[test]
fn file_scoped_namespace_is_the_package() {
    let f = run("using System;\nnamespace Acme.Billing.Core;\n\npublic class Invoice { }\n");
    assert_eq!(packages(&f), vec![("Acme.Billing.Core", 0, 2)]);
    // a file without a namespace has no package
    let f = run("class Invoice { }\n");
    assert!(packages(&f).is_empty());
}

#[test]
fn using_directives() {
    let f = run(r#"global using System.Linq;
using System;
using static System.Math;
using static System.Collections.Generic.List<int>;
using Json = Newtonsoft.Json.JsonConvert;
using Ints = System.Collections.Generic.List<int>;
using global::Acme.Util;
using Pair = (int, int);
namespace N
{
    using Acme.Inner;
    class C { }
}
"#);
    let ns = |spec: &str, line: u32| ImportFact {
        local: "*".into(),
        imported: "*".into(),
        spec: spec.into(),
        kind: ImportKind::Namespace,
        type_only: false,
        scope: 0,
        line,
    };
    assert_eq!(imp(&f, "System.Linq"), &ns("System.Linq", 1));
    assert_eq!(imp(&f, "System"), &ns("System", 2));
    assert_eq!(imp(&f, "Acme.Util"), &ns("Acme.Util", 7));
    assert_eq!(imp(&f, "Acme.Inner"), &ns("Acme.Inner", 11));

    let st = imp(&f, "System.Math");
    assert_eq!((st.local.as_str(), st.imported.as_str(), st.kind, st.line), ("*", "*", ImportKind::Static, 3));
    assert_eq!(imp(&f, "System.Collections.Generic.List").kind, ImportKind::Static);

    let json = imp(&f, "Newtonsoft.Json.JsonConvert");
    assert_eq!((json.local.as_str(), json.imported.as_str(), json.kind, json.scope, json.line), ("Json", "JsonConvert", ImportKind::Named, 0, 5));
    // alias to a generic type: generic arguments are stripped from the spec
    let ints = f.imports.iter().find(|i| i.local == "Ints").unwrap();
    assert_eq!((ints.imported.as_str(), ints.spec.as_str(), ints.kind), ("List", "System.Collections.Generic.List", ImportKind::Named));
    // aliases to tuples import nothing
    assert!(f.imports.iter().all(|i| i.local != "Pair"), "{:?}", f.imports);
    assert_eq!(f.imports.len(), 8, "{:?}", f.imports);
}

#[test]
fn scopes() {
    let f = run(r#"namespace N
{
    public class Svc
    {
        private int _n;
        public int Auto { get; set; }
        public int Body { get { return _n; } set { _n = value; } }
        public int Arrow => _n;
        public int this[int i] { get { return i; } }
        public event System.EventHandler Ev { add { } remove { } }
        public Svc() { }
        ~Svc() { }
        public static Svc operator +(Svc a, Svc b) => a;
        public static implicit operator int(Svc s) => 0;
        void Run()
        {
            {
                int inBlock = 0;
            }
            System.Func<int, int> f = x => x + 1;
            System.Action d = delegate { };
            int Local(int y) => y;
        }
    }
    struct S { }
    interface I { void M(); }
    record R(int A);
    enum E { One, Two }
}
"#);
    use ScopeKind::*;
    let kinds: Vec<ScopeKind> = f.scopes.iter().map(|s| s.kind).collect();
    assert_eq!(
        kinds,
        vec![
            File, Class, // Svc
            Function, Function, // Body get / set
            Function, // Arrow
            Function, // indexer get
            Function, Function, // event add / remove
            Function, Function, Function, Function, // ctor, dtor, operator +, conversion
            Function, Function, Function, Function, // Run, lambda, anonymous method, local function
            Class, Class, Function, Class, Class, // S, I, I.M, R, E
        ]
    );
    let parents: Vec<u32> = f.scopes.iter().skip(1).map(|s| s.parent).collect();
    assert_eq!(parents, vec![0, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 12, 12, 12, 0, 0, 17, 0, 0]);
    // the class scope spans the whole declaration
    let svc = f.scopes[1];
    assert_eq!((svc.start >> 32, svc.end >> 32), (3, 24));
    // blocks open no scope: the block-local belongs to `Run`
    assert_eq!(bind(&f, "inBlock").scope, 12);
}

#[test]
fn parameters_and_type_normalisation() {
    let f = run(r#"class C
{
    void M(Foo plain, Foo? nullable, A.B.Foo qualified, global::A.B.Foo rooted, List<Foo> generic,
           Foo[] array, (Foo, int) tuple, Foo* pointer, int prim, string str, object obj, dynamic dyn,
           ref Foo byRef, out Foo outP, this Foo self, @Foo @class, params Foo[] rest) { }
}
"#);
    let p = |n: &str| {
        let b = bind_kind(&f, n, BindKind::Param);
        assert_eq!(scope_kind(&f, b), ScopeKind::Function);
        b.decl_type.as_str()
    };
    assert_eq!(p("plain"), "Foo");
    assert_eq!(p("nullable"), "Foo");
    assert_eq!(p("qualified"), "A.B.Foo");
    assert_eq!(p("rooted"), "A.B.Foo");
    assert_eq!(p("generic"), "List");
    assert_eq!(p("array"), "");
    assert_eq!(p("tuple"), "");
    assert_eq!(p("pointer"), "Foo");
    assert_eq!(p("prim"), "");
    assert_eq!(p("str"), "");
    assert_eq!(p("obj"), "");
    assert_eq!(p("dyn"), "");
    assert_eq!(p("byRef"), "Foo");
    assert_eq!(p("outP"), "Foo");
    assert_eq!(p("self"), "Foo");
    // verbatim identifiers lose their `@`
    assert_eq!(p("class"), "Foo");
    assert_eq!(p("rest"), "");
    assert_eq!(bind(&f, "plain").line, 3);
    assert_eq!(bind(&f, "byRef").line, 5);
}

#[test]
fn fields_properties_events() {
    let f = run(r#"class Svc
{
    private readonly Foo _foo, _other;
    private static List<Bar> _bars = new();
    public Baz Prop { get; set; }
    public IRepo Repo { get; } = new SqlRepo();
    public Qux Computed => new Qux();
    public event EventHandler Changed;
    public event EventHandler Custom { add { } remove { } }
    public Item this[int index] { get { return null; } set { } }
}
"#);
    let field = |n: &str| {
        let b = bind_kind(&f, n, BindKind::Field);
        assert_eq!(b.scope, 1, "{b:?}");
        (b.decl_type.as_str(), b.line)
    };
    assert_eq!(field("_foo"), ("Foo", 3));
    assert_eq!(field("_other"), ("Foo", 3));
    assert_eq!(field("_bars"), ("List", 4));
    assert_eq!(field("Prop"), ("Baz", 5));
    assert_eq!(field("Repo"), ("IRepo", 6));
    assert_eq!(field("Computed"), ("Qux", 7));
    assert_eq!(field("Changed"), ("EventHandler", 8));
    assert_eq!(field("Custom"), ("EventHandler", 9));
    // the indexer is not a named member; its parameter and the setter's `value` live in the accessors
    let index: Vec<_> = f.bindings.iter().filter(|b| b.name == "index").collect();
    assert_eq!(index.len(), 2);
    assert!(index.iter().all(|b| b.kind == BindKind::Param && scope_kind(&f, b) == ScopeKind::Function));
    let values: Vec<&str> = f.bindings.iter().filter(|b| b.name == "value").map(|b| b.decl_type.as_str()).collect();
    assert_eq!(values, vec!["EventHandler", "EventHandler", "Item"]);
}

#[test]
fn primary_constructors() {
    let f = run(r#"public record Person(Name Name, int Age);
public record struct Point(Coord X);
public class Service(IRepo repo, ILogger<Service> log) { }
public struct Wrapper(Foo inner) { }
"#);
    let name = bind_kind(&f, "Name", BindKind::Param);
    assert_eq!((name.decl_type.as_str(), name.scope, name.line), ("Name", 1, 1));
    assert_eq!(scope_kind(&f, name), ScopeKind::Class);
    let name_field = bind_kind(&f, "Name", BindKind::Field);
    assert_eq!((name_field.decl_type.as_str(), name_field.scope), ("Name", 1));
    assert_eq!(bind_kind(&f, "Age", BindKind::Field).decl_type, "");
    assert_eq!(bind_kind(&f, "X", BindKind::Field).decl_type, "Coord");
    // class / struct primary-constructor parameters are parameters only
    for (n, t) in [("repo", "IRepo"), ("log", "ILogger"), ("inner", "Foo")] {
        let all: Vec<_> = f.bindings.iter().filter(|b| b.name == n).collect();
        assert_eq!(all.len(), 1, "{all:?}");
        assert_eq!((all[0].kind, all[0].decl_type.as_str()), (BindKind::Param, t));
        assert_eq!(scope_kind(&f, all[0]), ScopeKind::Class);
    }
}

#[test]
fn locals_and_initializers() {
    let f = run(r#"class C
{
    void Run()
    {
        Foo typed = Make();
        var created = new Foo();
        var generic = new Gen<Foo>(1);
        var rooted = new global::A.B.Foo();
        Foo targetTyped = new();
        var array = new Foo[3];
        var stat = Foo.Create();
        var free = Make();
        var member = other.Build();
        var self = this.repo.Load();
        var deep = A.B.Factory.Create();
        var genericCall = Make<Foo>();
        var genericRecv = Gen<Foo>.Create<Bar>();
        var chained = a.B().C();
        var awaited = await LoadAsync();
        var conditional = a?.B();
        var primitive = string.Join(",", xs);
        var cast = (Foo)obj;
        var asCast = obj as Foo;
        var paren = (new Foo());
        int i = 0, j = 1;
        using (var stream = new Stream()) { }
        using var reader = new Reader(stream);
        for (Foo it = First(); it != null; it = it.Next) { }
        try { } catch (IOException e) { } catch (Exception) { }
        var (left, right) = Pair();
    }
}
"#);
    assert_eq!(ty(&f, "typed"), ("Foo", "Make()"));
    assert_eq!(ty(&f, "created"), ("Foo", ""));
    assert_eq!(ty(&f, "generic"), ("Gen", ""));
    assert_eq!(ty(&f, "rooted"), ("A.B.Foo", ""));
    assert_eq!(ty(&f, "targetTyped"), ("Foo", ""));
    assert_eq!(ty(&f, "array"), ("", ""));
    assert_eq!(ty(&f, "stat"), ("", "Foo.Create()"));
    assert_eq!(ty(&f, "free"), ("", "Make()"));
    assert_eq!(ty(&f, "member"), ("", "other.Build()"));
    assert_eq!(ty(&f, "self"), ("", "this.repo.Load()"));
    assert_eq!(ty(&f, "deep"), ("", "A.B.Factory.Create()"));
    assert_eq!(ty(&f, "genericCall"), ("", "Make()"));
    assert_eq!(ty(&f, "genericRecv"), ("", "Gen.Create()"));
    assert_eq!(ty(&f, "chained"), ("", "a.B().C()"));
    assert_eq!(ty(&f, "awaited"), ("", ""));
    assert_eq!(ty(&f, "conditional"), ("", ""));
    assert_eq!(ty(&f, "primitive"), ("", ""));
    assert_eq!(ty(&f, "cast"), ("Foo", ""));
    assert_eq!(ty(&f, "asCast"), ("Foo", ""));
    assert_eq!(ty(&f, "paren"), ("Foo", ""));
    assert_eq!(ty(&f, "i"), ("", ""));
    assert_eq!(bind(&f, "j").line, 25);
    assert_eq!(ty(&f, "stream"), ("Stream", ""));
    assert_eq!(ty(&f, "reader"), ("Reader", ""));
    assert_eq!(ty(&f, "it"), ("Foo", "First()"));
    assert_eq!(ty(&f, "e"), ("IOException", ""));
    assert_eq!(ty(&f, "left"), ("", ""));
    assert_eq!(ty(&f, "right"), ("", ""));
    for b in f.bindings.iter().filter(|b| b.kind != BindKind::Package) {
        assert_eq!((b.kind, b.scope), (BindKind::Local, 2), "{b:?}");
    }
    assert_eq!(f.scopes[2].kind, ScopeKind::Function);
    assert_eq!(bind(&f, "typed").line, 5);
    assert_eq!(bind(&f, "right").line, 30);
}

#[test]
fn foreach_out_vars_and_patterns() {
    let f = run(r#"class C
{
    void Run(object obj)
    {
        foreach (Foo typed in xs) { }
        foreach (var untyped in xs) { }
        foreach (var (k, v) in pairs) { }
        if (TryGet(out Foo outTyped, out var outVar)) { }
        if (obj is Foo isTyped) { }
        if (obj is var isVar) { }
        if (obj is Bar { Size: > 1 } recursive) { }
        if (obj is not null and Baz combined) { }
        switch (obj)
        {
            case Qux caseTyped:
                break;
            case List<Foo> caseGeneric when caseGeneric.Count > 0:
                break;
        }
        var r = obj switch { Foo armTyped => 1, _ => 0 };
    }
}
"#);
    assert_eq!(ty(&f, "typed"), ("Foo", ""));
    assert_eq!(ty(&f, "untyped"), ("", ""));
    assert_eq!(ty(&f, "k"), ("", ""));
    assert_eq!(ty(&f, "v"), ("", ""));
    assert_eq!(ty(&f, "outTyped"), ("Foo", ""));
    assert_eq!(ty(&f, "outVar"), ("", ""));
    assert_eq!(ty(&f, "isTyped"), ("Foo", ""));
    assert_eq!(ty(&f, "isVar"), ("", ""));
    assert_eq!(ty(&f, "recursive"), ("Bar", ""));
    assert_eq!(ty(&f, "combined"), ("Baz", ""));
    assert_eq!(ty(&f, "caseTyped"), ("Qux", ""));
    assert_eq!(ty(&f, "caseGeneric"), ("List", ""));
    assert_eq!(ty(&f, "armTyped"), ("Foo", ""));
    assert_eq!(bind(&f, "typed").line, 5);
    assert_eq!(bind(&f, "outVar").line, 8);
    assert_eq!(bind(&f, "caseTyped").line, 15);
    for n in ["typed", "outTyped", "isTyped", "caseTyped", "armTyped"] {
        let b = bind(&f, n);
        assert_eq!((b.kind, b.scope), (BindKind::Local, 2), "{b:?}");
    }
}

#[test]
fn lambdas_anonymous_methods_and_local_functions() {
    let f = run(r#"class C
{
    void Run()
    {
        items.Select(single => single.Name);
        items.Select((Foo typed, int index) => typed);
        items.Aggregate((acc, next) => acc);
        handler = delegate (Bar arg) { var inside = new Baz(); };
        int Helper(Qux q) { return 0; }
        static void Other() { }
    }
}
"#);
    let run_scope = 2;
    assert_eq!(f.scopes[run_scope as usize].kind, ScopeKind::Function);
    let param = |n: &str| {
        let b = bind_kind(&f, n, BindKind::Param);
        assert_eq!(scope_kind(&f, b), ScopeKind::Function);
        assert_eq!(f.scopes[b.scope as usize].parent, run_scope, "{b:?}");
        (b.decl_type.as_str(), b.line)
    };
    assert_eq!(param("single"), ("", 5));
    assert_eq!(param("typed"), ("Foo", 6));
    assert_eq!(param("index"), ("", 6));
    assert_eq!(param("acc"), ("", 7));
    assert_eq!(param("next"), ("", 7));
    assert_eq!(param("arg"), ("Bar", 8));
    assert_eq!(param("q"), ("Qux", 9));
    // locals of an anonymous method live in its own scope
    let inside = bind(&f, "inside");
    assert_eq!((inside.scope, inside.decl_type.as_str()), (bind(&f, "arg").scope, "Baz"));
    // local functions bind in the enclosing function scope (hoisted: visible from line 0)
    for n in ["Helper", "Other"] {
        let b = bind_kind(&f, n, BindKind::LocalFn);
        assert_eq!((b.scope, b.line), (run_scope, 0), "{b:?}");
    }
    assert_ne!(bind(&f, "q").scope, run_scope);
}

#[test]
fn query_range_variables() {
    let f = run(r#"class C
{
    void Run()
    {
        var joined = from x in xs
                     join Bar y in ys on x.Id equals y.Id into g
                     select g;
        var plain = from a in xs
                    join b in ys on a.Id equals b.Id
                    select b;
        var continued = from c in xs
                        let n = c.Name
                        select n into w
                        where w != null
                        select w;
    }
}
"#);
    assert_eq!(ty(&f, "x"), ("", ""));
    assert_eq!(ty(&f, "y"), ("Bar", ""));
    // a group join's `into g` is a `join_into_clause`, not a bare `into` token
    assert_eq!((ty(&f, "g"), bind(&f, "g").line), (("", ""), 6));
    assert_eq!(ty(&f, "b"), ("", ""));
    assert_eq!(bind(&f, "n").line, 12);
    assert_eq!(bind(&f, "w").line, 13);
    for n in ["x", "y", "g", "a", "b", "c", "n", "w"] {
        assert_eq!(bind(&f, n).kind, BindKind::Local, "{n}");
    }
    assert_eq!(f.bindings.iter().filter(|b| b.name == "g").count(), 1, "{:?}", f.bindings);
}

#[test]
fn deeply_nested_blocks_and_expressions() {
    let depth = 4000;
    let mut src = String::from("class C\n{\n    void Deep(Foo top)\n    {\n");
    for i in 0..depth {
        src.push_str(&format!("{{ Foo v{i} = new Foo();\n"));
    }
    src.push_str(&"}".repeat(depth));
    src.push_str("\n    }\n    void After(Bar b)\n    {\n        var x = ");
    src.push_str(&"(".repeat(5000));
    src.push('1');
    src.push_str(&")".repeat(5000));
    src.push_str(";\n        var y = ");
    src.push_str(&"(".repeat(5000));
    src.push_str("new Baz()");
    src.push_str(&")".repeat(5000));
    src.push_str(";\n        var (p, ");
    src.push_str(&"(p, ".repeat(3000));
    src.push_str("last");
    src.push_str(&")".repeat(3000));
    src.push_str(") = Pair();\n        Func<int, int> f = ");
    src.push_str(&"a => ".repeat(3000));
    src.push_str("a;\n        var z = new Qux();\n    }\n}\n");
    // run on a thread with a modest stack: the walker must bound its recursion
    let f = std::thread::Builder::new().stack_size(2 << 20).spawn(move || run(&src)).unwrap().join().unwrap();
    assert_eq!(ty(&f, "top"), ("Foo", ""));
    assert_eq!(ty(&f, "v0"), ("Foo", ""));
    assert_eq!(ty(&f, "b"), ("Bar", ""));
    assert_eq!(bind(&f, "x").kind, BindKind::Local);
    // parentheses past the chain bound hide the constructor rather than crashing
    assert_eq!(bind(&f, "y").kind, BindKind::Local);
    assert_eq!(bind(&f, "f").kind, BindKind::Local);
    assert_eq!(ty(&f, "z"), ("Qux", ""));
    assert!(!f.bindings.iter().any(|b| b.name == format!("v{}", depth - 1)));
    // the deepest designation is cut off, the outer ones are bound
    assert!(f.bindings.iter().any(|b| b.name == "p"));
    assert!(!f.bindings.iter().any(|b| b.name == "last"));
}

#[test]
fn top_level_statements() {
    let f = run(r#"using System;
var app = new App();
Foo typed = Build();
void Helper(Bar b) { }
"#);
    assert_eq!((bind(&f, "app").scope, bind(&f, "app").decl_type.as_str()), (0, "App"));
    assert_eq!(ty(&f, "typed"), ("Foo", "Build()"));
    let helper = bind_kind(&f, "Helper", BindKind::LocalFn);
    assert_eq!(helper.scope, 0);
    let b = bind_kind(&f, "b", BindKind::Param);
    assert_eq!((b.decl_type.as_str(), f.scopes[b.scope as usize].parent), ("Bar", 0));
}

#[test]
fn malformed_and_empty_input_do_not_panic() {
    for src in [
        "",
        "   \n\n",
        "using ;\nusing static ;\nusing X = ;\nnamespace ;\n",
        "namespace A.B {\n class C {\n void M(Foo x, { var y = new ; foreach (var in ) {} \n",
        "class C { void M() { if (x is ) {} switch (o) { case : } var (a, = ; out var ; } }",
        "record R(;\nclass D(Foo a { public Foo P { get; set } public int this[ { get { } } }\n",
        "}}}}{{{{ => => delegate ( @ @@ ::: global:: using = ;",
        "class C { void M() { Func<int> f = (Foo a, ) => ; int L( { } } }",
    ] {
        let _ = run(src);
    }
    // a partially broken file still yields the facts of its intact parts
    let f = run("namespace Ok.Ns;\nusing System;\nclass C {\n void M(Foo x) { var y = new Bar(); }\n void Broken( { }\n}\n");
    assert_eq!(packages(&f), vec![("Ok.Ns", 0, 1)]);
    assert_eq!(imp(&f, "System").kind, ImportKind::Namespace);
    assert_eq!(bind(&f, "x").decl_type, "Foo");
    assert_eq!(bind(&f, "y").decl_type, "Bar");
}
