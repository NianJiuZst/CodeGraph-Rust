//! Incremental-sync equivalence tests (`incremental == full rebuild`) for Vue & Svelte SFCs,
//! including `@/` / `$lib/` imports, Options API edits, and Svelte `$store` invalidation.

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
fn vue_and_svelte_incremental_sync_matches_full_rebuild() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();

    write(root, "package.json", r#"{ "dependencies": { "vue": "^3.4.0", "svelte": "^5.0.0" } }"#, 1);
    write(root, "src/utils.ts", "export function fmt(x: number) { return String(x); }\n", 1);
    write(root, "src/lib/stores.ts", "export const counter = { subscribe() {} };\nexport const globalCounter = { subscribe() {} };\n", 1);
    write(
        root,
        "src/components/Btn.vue",
        "<script setup lang=\"ts\">\nconst p = defineProps<{ text: string }>();\n</script>\n<template><button>{{ p.text }}</button></template>\n",
        1,
    );
    write(
        root,
        "src/App.vue",
        "<script setup lang=\"ts\">\nimport Btn from '@/components/Btn.vue';\nimport { fmt } from '@/utils';\n</script>\n<template><Btn text=\"ok\" /><span>{{ fmt(1) }}</span></template>\n",
        1,
    );
    write(root, "src/Widget.svelte", "<script lang=\"ts\">\nimport { counter } from '$lib/stores';\n</script>\n<p>{$counter} {$globalCounter}</p>\n", 1);

    index_project(root, &opts()).unwrap();
    assert_same_as_rebuild(root, "initial");

    // Step 2: modify store definitions (`globalCounter` removed, `counter` renamed -> `total`)
    write(root, "src/lib/stores.ts", "export const total = { subscribe() {} };\n", 2);
    sync(root, &SyncOptions { index: opts(), ..Default::default() }).unwrap();
    assert_same_as_rebuild(root, "svelte store changed");

    // Step 3: restore `globalCounter` without touching Widget.svelte (tests `$store` tail invalidation)
    write(root, "src/lib/stores.ts", "export const total = { subscribe() {} };\nexport const globalCounter = { subscribe() {} };\n", 3);
    sync(root, &SyncOptions { index: opts(), ..Default::default() }).unwrap();
    assert_same_as_rebuild(root, "svelte global store restored");

    // Step 4: edit Vue component `Btn.vue` into Options API & change `fmt` export
    write(
        root,
        "src/components/Btn.vue",
        "<template><button @click=\"press\">Go</button></template>\n<script lang=\"ts\">\nimport { defineComponent } from 'vue';\nexport default defineComponent({ methods: { press() {} } });\n</script>\n",
        4,
    );
    write(root, "src/utils.ts", "export function fmt(x: number): string { return `#${x}`; }\n", 4);
    sync(root, &SyncOptions { index: opts(), ..Default::default() }).unwrap();
    assert_same_as_rebuild(root, "vue edited");

    // Step 5: remove `Btn.vue` (invalidates `App.vue` import + template component ref)
    std::fs::remove_file(root.join("src/components/Btn.vue")).unwrap();
    sync(root, &SyncOptions { index: opts(), ..Default::default() }).unwrap();
    assert_same_as_rebuild(root, "vue component deleted");
}
