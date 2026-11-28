//! Framework rules (design doc 6.7): detection from manifests, extract hooks that add route
//! nodes, resolve rules. The hook list is empty for projects that use none.

pub mod aspnet;
pub mod href;
pub mod jsroutes;
pub mod nav;
pub mod nest;
pub mod routes;

use crate::project::ProjectInfo;
use cg_extract::FileHook;
use routes::{Framework, RouteHook};
use std::sync::Arc;

pub fn hooks(project: &ProjectInfo) -> Vec<Arc<dyn FileHook>> {
    // synthesis site scanners: cheap literal prefilters keep them off files that cannot match
    let mut v: Vec<Arc<dyn FileHook>> = vec![
        Arc::new(crate::synth::sites::SiteScanner),
        Arc::new(crate::synth::sites::GoTypeSites),
        Arc::new(crate::synth::gin::GinScanner),
        Arc::new(crate::synth::js_proto::InheritScanner),
        Arc::new(crate::synth::redux::ThunkScanner),
        Arc::new(crate::synth::jvm::TestRequestScanner),
        Arc::new(crate::synth::bridges::BridgeScanner),
        Arc::new(crate::synth::dynamic::CallbackScanner),
        Arc::new(crate::synth::dynamic::RegistryScanner),
        Arc::new(crate::synth::dynamic::TierScanner),
        Arc::new(crate::synth::dynamic::CeleryScanner),
        Arc::new(crate::synth::dynamic::SpringEventScanner),
        Arc::new(crate::synth::dynamic::MediatrScanner),
    ];
    let frameworks = project.detect_frameworks();
    if frameworks.iter().any(|f| matches!(f.as_str(), "next" | "react-router" | "tanstack-router")) {
        v.push(Arc::new(nav::NavScanner));
    }
    if frameworks.iter().any(|f| f == "next") {
        v.push(Arc::new(nav::ProxyScanner));
    }
    if frameworks.iter().any(|f| f == "nestjs") {
        v.push(Arc::new(nest::NestScanner));
    }
    for name in frameworks {
        let fw = match name.as_str() {
            "express" => Framework::Express,
            "flask" => Framework::Flask,
            "fastapi" => Framework::FastApi,
            "django" => Framework::Django,
            "gin" => Framework::Gin,
            "spring" => Framework::Spring,
            "aspnet-core" => Framework::AspNetCore,
            "axum" => Framework::Axum,
            "next" => Framework::Next,
            "nuxt" => Framework::Nuxt,
            "sveltekit" => Framework::SvelteKit,
            "nestjs" => Framework::Nest,
            "react-router" => Framework::ReactRouter,
            "tanstack-router" => Framework::TanStack,
            _ => continue,
        };
        v.push(Arc::new(RouteHook { fw }));
    }
    v
}

/// Resolve a JS binding using the project's import/export tables, including barrel exports.
pub(crate) fn js_symbol(e: &crate::engine::Engine<'_>, file: cg_core::FileId, name: &str) -> Option<cg_core::NodeIdx> {
    use crate::engine::Bound;
    let view = e.view;
    let (head, tail) = name.split_once('.').map_or((name, None), |(h, t)| (h, Some(t)));
    if head.is_empty() || !head.chars().all(|c| c.is_alphanumeric() || c == '_' || c == '$') {
        return None;
    }
    if let Some(i) = view.imports_of(file).iter().position(|imp| view.text_str(imp.local) == head) {
        return match (e.bind_import(file, i), tail) {
            (Bound::Node(n), None) => Some(n),
            (Bound::Module(f), Some(member)) if !member.contains('.') => match e.lookup_export(f, member, 0) {
                Some(Bound::Node(n)) => Some(n),
                _ => None,
            },
            _ => None,
        };
    }
    tail.is_none().then(|| e.lookup_top_level(file, head)).flatten()
}
