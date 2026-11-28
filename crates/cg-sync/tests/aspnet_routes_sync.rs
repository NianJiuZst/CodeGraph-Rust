//! Incremental-sync equivalence tests (`incremental == full rebuild`) for ASP.NET Core
//! route extraction, Minimal API cross-file handlers, frontend HTTP route linking, and
//! `.csproj` manifest invalidation.

use cg_resolve::{index_project, IndexOptions};
use cg_store::{dump, Store};
use cg_sync::{sync, SyncOptions};
use std::path::Path;
use std::time::{Duration, SystemTime};

fn opts() -> IndexOptions {
    IndexOptions { threads: Some(2), ..Default::default() }
}

fn write(root: &Path, rel: &str, text: &str, step: u64) {
    let path = root.join(rel);
    if let Some(p) = path.parent() {
        std::fs::create_dir_all(p).unwrap();
    }
    std::fs::write(&path, text).unwrap();
    let t = SystemTime::UNIX_EPOCH + Duration::from_secs(1_700_000_000 + step * 10);
    std::fs::OpenOptions::new().write(true).open(&path).unwrap().set_modified(t).unwrap();
}

fn assert_same_as_rebuild(root: &Path, step: &str) {
    let synced = Store::locate(root).unwrap().open_snapshot().unwrap();
    let fresh = cg_resolve::build_seg(root, &opts()).unwrap().seg;
    let fresh_view = cg_store::View::new(std::sync::Arc::new(fresh)).with_root(root.to_path_buf());
    let a = dump::canonical(&synced.view);
    let b = dump::canonical(&fresh_view);
    assert_eq!(a, b, "incremental sync diverged from full rebuild at step: {step}");
}

#[test]
fn aspnet_core_routes_and_csproj_edits_match_full_rebuild() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();

    let web_csproj = "<Project Sdk=\"Microsoft.NET.Sdk.Web\">\n  <PropertyGroup><TargetFramework>net8.0</TargetFramework></PropertyGroup>\n</Project>\n";
    let plain_csproj = "<Project Sdk=\"Microsoft.NET.Sdk\">\n  <PropertyGroup><OutputType>Exe</OutputType></PropertyGroup>\n</Project>\n";

    write(root, "Api.csproj", web_csproj, 1);
    write(
        root,
        "Controllers/OrdersController.cs",
        r#"namespace Demo.Controllers;
[ApiController]
[Route("api/[controller]")]
public class OrdersController : ControllerBase
{
    [HttpGet("{id:int}")]
    public string GetById(int id) => "ok";
}
"#,
        1,
    );
    write(
        root,
        "Handlers/OrderHandlers.cs",
        "namespace Demo.Handlers;\npublic static class OrderHandlers {\n    public static string Create() => \"ok\";\n}\n",
        1,
    );
    write(
        root,
        "Endpoints/OrderEndpoints.cs",
        r#"namespace Demo.Endpoints;
using Demo.Handlers;
public static class OrderEndpoints
{
    public static void Map(WebApplication app)
    {
        var g = app.MapGroup("/api/v1");
        g.MapPost("/orders", OrderHandlers.Create);
    }
}
"#,
        1,
    );
    write(root, "web/client.ts", "export function load(id: number) { return fetch(`/api/orders/${id}`); }\n", 1);

    index_project(root, &opts()).unwrap();
    assert_same_as_rebuild(root, "initial");

    // Step 2: change controller prefix and add an action.
    write(
        root,
        "Controllers/OrdersController.cs",
        r#"namespace Demo.Controllers;
[ApiController]
[Route("api/v2/[controller]")]
public class OrdersController : ControllerBase
{
    [HttpGet("{id:int}")]
    public string GetById(int id) => "ok";

    [HttpDelete("{id:int}")]
    public void Delete(int id) {}
}
"#,
        2,
    );
    sync(root, &SyncOptions { index: opts(), ..Default::default() }).unwrap();
    assert_same_as_rebuild(root, "controller routes edited");

    // Step 3: rename handler in OrderHandlers.cs without touching OrderEndpoints.cs.
    write(
        root,
        "Handlers/OrderHandlers.cs",
        "namespace Demo.Handlers;\npublic static class OrderHandlers {\n    public static string CreateV2() => \"ok\";\n}\n",
        3,
    );
    sync(root, &SyncOptions { index: opts(), ..Default::default() }).unwrap();
    assert_same_as_rebuild(root, "minimal api handler removed");

    // Step 4: restore Create handler and change MapGroup prefix.
    write(
        root,
        "Handlers/OrderHandlers.cs",
        "namespace Demo.Handlers;\npublic static class OrderHandlers {\n    public static string Create() => \"ok\";\n}\n",
        4,
    );
    write(
        root,
        "Endpoints/OrderEndpoints.cs",
        r#"namespace Demo.Endpoints;
using Demo.Handlers;
public static class OrderEndpoints
{
    public static void Map(WebApplication app)
    {
        var g = app.MapGroup("/api/v2");
        g.MapPost("/orders", OrderHandlers.Create);
    }
}
"#,
        4,
    );
    sync(root, &SyncOptions { index: opts(), ..Default::default() }).unwrap();
    assert_same_as_rebuild(root, "minimal api group updated");

    // Step 5: switch `.csproj` to plain non-web SDK (config digest change drops ASP.NET Core routes).
    write(root, "Api.csproj", plain_csproj, 5);
    sync(root, &SyncOptions { index: opts(), ..Default::default() }).unwrap();
    assert_same_as_rebuild(root, "csproj switched to non-web sdk");

    // Step 6: restore Web SDK in `.csproj` (re-enables ASP.NET Core routes).
    write(root, "Api.csproj", web_csproj, 6);
    sync(root, &SyncOptions { index: opts(), ..Default::default() }).unwrap();
    assert_same_as_rebuild(root, "csproj restored to web sdk");
}
