//! End-to-end extraction & resolution tests for Vue (`.vue`) and Svelte (`.svelte`) SFCs.

use cg_core::Lang;
use cg_resolve::{build_seg, IndexOptions};
use cg_store::{dump, status, View};
use std::sync::Arc;

fn project(files: &[(&str, &str)]) -> (tempfile::TempDir, View) {
    let dir = tempfile::tempdir().unwrap();
    for (rel, body) in files {
        let p = dir.path().join(rel);
        if let Some(parent) = p.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(&p, body).unwrap();
    }
    let seg = build_seg(dir.path(), &IndexOptions { threads: Some(2), ..Default::default() }).unwrap().seg;
    let view = View::new(Arc::new(seg)).with_root(dir.path().to_path_buf());
    (dir, view)
}

fn has(edges: &[String], needle: &str) {
    assert!(edges.iter().any(|e| e == needle), "missing {needle}\n{edges:#?}");
}

fn lacks(edges: &[String], needle: &str) {
    assert!(!edges.iter().any(|e| e == needle), "unexpected {needle}\n{edges:#?}");
}

#[test]
fn vue_script_setup_template_aliases_and_nuxt_routes() {
    let (_dir, view) = project(&[
        ("package.json", r#"{ "dependencies": { "vue": "^3.4.0", "nuxt": "^3.12.0" } }"#),
        (
            "src/utils/format.ts",
            r#"export function formatPrice(n: number): string {
    return `$${n}`;
}
export function loadItems(): number[] {
    return [1, 2, 3];
}
"#,
        ),
        (
            "src/components/MyButton.vue",
            r#"<script setup lang="ts" generic="T extends Record<string, unknown>">
const props = defineProps<{ label: string; value?: T }>();
const emit = defineEmits<{ click: [] }>();
</script>
<template>
  <!-- 按钮组件 -->
  <button>{{ props.label }}</button>
</template>
"#,
        ),
        ("src/components/index.ts", "export { default as ActionButton } from './MyButton.vue';\n"),
        (
            "src/pages/blog/[slug].vue",
            r#"<script setup lang="ts">
import { ActionButton } from '@/components';
import MyButton from '@/components/MyButton';
import { formatPrice, loadItems } from '@/utils/format';
import { ref } from '#imports';

const route = useRoute();
function handleSave() {
    return formatPrice(42);
}
</script>
<template>
  <!-- 页面内容：测试中文注释与插值 -->
  <section>
    <ActionButton @click="handleSave" />
    <my-button />
    <Transition>
      <div v-for="item in loadItems()">
        <span>{{ formatPrice(item) }}</span>
        <span>{{ item.toString() }}</span>
      </div>
    </Transition>
  </section>
</template>
"#,
        ),
    ]);

    let edges = dump::dump_edges(&view);

    // Language detection
    let page_file = view.files().find(|f| view.file(*f).path() == "src/pages/blog/[slug].vue").expect("page file");
    assert_eq!(view.file(page_file).lang(), Lang::Vue);

    // Imports & barrel re-exports of .vue components + `@/` alias
    has(&edges, "src/pages/blog/[slug].vue -imports-> src/components/index.ts");
    has(&edges, "src/pages/blog/[slug].vue -imports-> src/components/MyButton.vue");
    has(&edges, "src/pages/blog/[slug].vue -imports-> src/utils/format.ts");

    // Template component tags (PascalCase through barrel AND kebab-case -> PascalCase)
    has(&edges, "src/pages/blog/[slug].vue::[slug] -references-> src/components/MyButton.vue::MyButton");

    // Template calls: bare `@click="handleSave"`, `v-for` source `loadItems()`, interpolation `{{ formatPrice(item) }}`
    has(&edges, "src/pages/blog/[slug].vue::[slug] -calls-> src/pages/blog/[slug].vue::handleSave");
    has(&edges, "src/pages/blog/[slug].vue::[slug] -calls-> src/utils/format.ts::loadItems");
    has(&edges, "src/pages/blog/[slug].vue::[slug] -calls-> src/utils/format.ts::formatPrice");
    has(&edges, "src/pages/blog/[slug].vue::handleSave -calls-> src/utils/format.ts::formatPrice");

    // Nuxt file-based page route -> page Component
    has(&edges, "src/pages/blog/[slug].vue::src/pages/blog/[slug].vue::route:/blog/:slug -calls-> src/pages/blog/[slug].vue::[slug]");

    // Vue compiler macros & Nuxt auto-imports marked BUILTIN (not NO_CANDIDATE)
    let btn_file = view.files().find(|f| view.file(*f).path() == "src/components/MyButton.vue").expect("btn file");
    for macro_name in ["defineProps", "defineEmits"] {
        let r = view.refs_of_file(btn_file).iter().find(|r| view.text_str(r.name) == macro_name).expect(macro_name);
        assert_eq!(r.status, status::BUILTIN, "{macro_name} should be BUILTIN");
    }
    let use_route_ref = view.refs_of_file(page_file).iter().find(|r| view.text_str(r.name) == "useRoute").expect("useRoute");
    assert_eq!(use_route_ref.status, status::BUILTIN, "Nuxt useRoute should be BUILTIN");

    // `#imports` virtual module marked EXTERNAL (not NOT_FOUND)
    let virt_ref = view.refs_of_file(page_file).iter().find(|r| view.text_str(r.name) == "#imports").expect("#imports");
    assert_eq!(virt_ref.status, status::EXTERNAL);

    // Built-in `<Transition>` must not emit a phantom reference
    assert!(!view.refs_of_file(page_file).iter().any(|r| view.text_str(r.name) == "Transition"));
}

#[test]
fn vue_options_api_methods_this_calls_and_self_closing_script() {
    let (_dir, view) = project(&[
        ("src/api.ts", "export function submitLogin(user: string) { return user; }\n"),
        (
            "src/LoginForm.vue",
            r#"<template>
  <form @submit.prevent="handleLogin">
    <p>{{ displayTitle() }}</p>
    <script src="/analytics.js" />
  </form>
</template>
<script lang="ts">
import { defineComponent } from 'vue';
import { submitLogin } from './api';

export default defineComponent({
  computed: {
    displayTitle() {
      return 'Login';
    },
  },
  methods: {
    validate() {
      return true;
    },
    handleLogin() {
      if (this.validate()) {
        submitLogin('alice');
      }
    },
  },
  mounted() {
    this.validate();
  },
});
</script>
"#,
        ),
    ]);

    let edges = dump::dump_edges(&view);

    // Options API method nodes + `this.validate()` inside methods & lifecycle hooks
    has(&edges, "src/LoginForm.vue::LoginForm::handleLogin -calls-> src/LoginForm.vue::LoginForm::validate");
    has(&edges, "src/LoginForm.vue::LoginForm::handleLogin -calls-> src/api.ts::submitLogin");
    has(&edges, "src/LoginForm.vue::LoginForm::mounted -calls-> src/LoginForm.vue::LoginForm::validate");

    // Template calls to own Options API methods link directly from Component -> Method
    has(&edges, "src/LoginForm.vue::LoginForm -calls-> src/LoginForm.vue::LoginForm::handleLogin");
    has(&edges, "src/LoginForm.vue::LoginForm -calls-> src/LoginForm.vue::LoginForm::displayTitle");

    // Self-closing `<script src="/analytics.js" />` did not corrupt the real `<script>` block
    let login_file = view.files().find(|f| view.file(*f).path() == "src/LoginForm.vue").unwrap();
    assert_eq!(view.file(login_file).flags() & cg_core::fileflags::PARSE_ERROR, 0);
}

#[test]
fn svelte5_runes_stores_lib_alias_and_sveltekit_routes() {
    let (_dir, view) = project(&[
        ("package.json", r#"{ "dependencies": { "svelte": "^5.0.0", "@sveltejs/kit": "^2.0.0" } }"#),
        (
            "src/lib/stores.ts",
            r#"export const count = { subscribe() {} };
export function formatCount(n: number): string {
    return String(n);
}
"#,
        ),
        (
            "src/lib/counter.svelte.ts",
            r#"export function makeCounter() {
    let n = $state(0);
    const doubled = $derived.by(() => n * 2);
    return { n, doubled };
}
"#,
        ),
        (
            "src/lib/Badge.svelte",
            r#"<script module lang="ts">
export function badgeLabel(x: string): string {
    return x.toUpperCase();
}
</script>
<script lang="ts">
let { text } = $props();
</script>
<span>{badgeLabel(text)}</span>
"#,
        ),
        (
            "src/routes/posts/[id=integer]/+page.svelte",
            r#"<script lang="ts">
import { goto } from '$app/navigation';
import { count, formatCount } from '$lib/stores';
import Badge, { badgeLabel } from '$lib/Badge.svelte';

let local = $state.raw(1);
$effect.pre(() => {
    formatCount(local);
});

function handleClick() {
    goto('/next');
    return badgeLabel('ok');
}
</script>

<Badge text="hi" />
<button onclick={handleClick}>{formatCount($count)}</button>
<p>{$count}</p>
"#,
        ),
    ]);

    let edges = dump::dump_edges(&view);

    // `$lib/` alias resolution & default + named exports from `.svelte`
    has(&edges, "src/routes/posts/[id=integer]/+page.svelte -imports-> src/lib/stores.ts");
    has(&edges, "src/routes/posts/[id=integer]/+page.svelte -imports-> src/lib/Badge.svelte");

    // Template component tag `<Badge />` -> Badge Component
    has(&edges, "src/routes/posts/[id=integer]/+page.svelte::+page -references-> src/lib/Badge.svelte::Badge");

    // Template calls: `onclick={handleClick}` and `{formatCount($count)}` + module-script export `badgeLabel`
    has(&edges, "src/routes/posts/[id=integer]/+page.svelte::+page -calls-> src/routes/posts/[id=integer]/+page.svelte::handleClick");
    has(&edges, "src/routes/posts/[id=integer]/+page.svelte::+page -calls-> src/lib/stores.ts::formatCount");
    has(&edges, "src/routes/posts/[id=integer]/+page.svelte::handleClick -calls-> src/lib/Badge.svelte::badgeLabel");
    has(&edges, "src/lib/Badge.svelte::Badge -calls-> src/lib/Badge.svelte::badgeLabel");

    // Svelte `$count` auto-subscription -> `count` constant in `stores.ts`
    has(&edges, "src/routes/posts/[id=integer]/+page.svelte::+page -references-> src/lib/stores.ts::count");

    // SvelteKit route `/posts/:id` (with `=integer` matcher stripped) -> `+page` Component
    has(
        &edges,
        "src/routes/posts/[id=integer]/+page.svelte::src/routes/posts/[id=integer]/+page.svelte::route:/posts/:id -calls-> src/routes/posts/[id=integer]/+page.svelte::+page",
    );

    // `$app/navigation` virtual module -> EXTERNAL
    let page_file = view.files().find(|f| view.file(*f).path() == "src/routes/posts/[id=integer]/+page.svelte").unwrap();
    let app_ref = view.refs_of_file(page_file).iter().find(|r| view.text_str(r.name) == "$app/navigation").expect("$app/navigation");
    assert_eq!(app_ref.status, status::EXTERNAL);

    // Svelte 5 runes (`$props`, `$state`, `$state.raw`, `$derived.by`, `$effect.pre`) -> BUILTIN
    let rune_file = view.files().find(|f| view.file(*f).path() == "src/lib/counter.svelte.ts").unwrap();
    for name in ["$state", "$derived.by"] {
        let r = view.refs_of_file(rune_file).iter().find(|r| view.text_str(r.name) == name).expect(name);
        assert_eq!(r.status, status::BUILTIN, "{name} should be BUILTIN in .svelte.ts");
    }
    for name in ["$state.raw", "$effect.pre"] {
        let r = view.refs_of_file(page_file).iter().find(|r| view.text_str(r.name) == name).expect(name);
        assert_eq!(r.status, status::BUILTIN, "{name} should be BUILTIN in .svelte");
    }

    // No spurious React JSX edges synthesized on SFC files
    lacks(&edges, "src/routes/posts/[id=integer]/+page.svelte::+page -calls-> src/lib/Badge.svelte::Badge");
}

#[test]
fn sfc_script_html_comment_literal_and_tsconfig_lib_override() {
    let (_dir, view) = project(&[
        ("tsconfig.json", r#"{ "compilerOptions": { "paths": { "$lib/*": ["shared/*"] } } }"#),
        ("shared/api.ts", "export function loadData() { return 1; }\n"),
        ("src/lib/api.ts", "export function wrongData() { return 2; }\n"),
        (
            "src/Page.svelte",
            r#"<script lang="ts">
import { loadData } from '$lib/api';
const marker = '<!-- not an html comment';
export function init() { return loadData() + marker.length; }
</script>
<!-- real comment -->
<p>{init()}</p>
"#,
        ),
    ]);
    let edges = dump::dump_edges(&view);
    // Explicit tsconfig `$lib/* -> shared/*` must win over SvelteKit's default `src/lib/*`.
    has(&edges, "src/Page.svelte -imports-> shared/api.ts");
    lacks(&edges, "src/Page.svelte -imports-> src/lib/api.ts");
    // Script block containing `'<!--'` string literal must remain intact and resolve calls.
    has(&edges, "src/Page.svelte::init -calls-> shared/api.ts::loadData");
}
