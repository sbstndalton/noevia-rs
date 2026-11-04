#![allow(clippy::unwrap_used)]
use proc_macro2::{TokenStream, TokenTree};
use std::path::Path;
use syn::visit::Visit;
fn unsafe_tokens(stream: TokenStream) -> usize {
    stream
        .into_iter()
        .map(|token| match token {
            TokenTree::Ident(id) => usize::from(id == "unsafe"),
            TokenTree::Group(group) => unsafe_tokens(group.stream()),
            _ => 0,
        })
        .sum()
}
// Explicit review-visible budgets: blocks, unsafe contracts, unsafe tokens,
// lint escapes, ABI attributes. No hashes; harmless edits remain allowed.
const ALLOWLIST: &[(&str, [usize; 5])] = &[
    ("crates/mem-return/src/lib.rs", [1, 0, 1, 1, 0]),
    ("crates/gguf-memtest/tests/hostile.rs", [2, 3, 5, 1, 0]),
    ("crates/sqlite-vec-loader/src/lib.rs", [2, 0, 4, 1, 0]),
    ("bins/dav-parse-wasm/src/lib.rs", [0, 0, 0, 49, 50]),
    ("bins/sandbox-bridge-wasm/src/lib.rs", [0, 0, 0, 10, 10]),
];
#[derive(Default)]
struct Guard {
    blocks: usize,
    contracts: usize,
    escapes: usize,
    abi: usize,
    hidden: usize,
    location: Option<(std::path::PathBuf, std::path::PathBuf)>,
}
impl Guard {
    fn reference_ok(&self, literal: &syn::LitStr) -> bool {
        if !literal.value().ends_with(".rs") {
            return false;
        }
        self.location.as_ref().is_some_and(|(root, file)| {
            file.parent()
                .and_then(|dir| dir.join(literal.value()).canonicalize().ok())
                .is_some_and(|path| path.starts_with(root))
        })
    }
}
impl<'ast> Visit<'ast> for Guard {
    fn visit_macro(&mut self, node: &'ast syn::Macro) {
        if node.path.is_ident("include") {
            let generated =
                self.location.as_ref().is_some_and(|(root, file)| {
                    *file == root.join("bins/noevia-server/src/routes.rs")
                }) && node.tokens.to_string().replace(' ', "")
                    == "concat!(env!(\"OUT_DIR\"),\"/routes_gen.rs\")";
            if !generated
                && !syn::parse2::<syn::LitStr>(node.tokens.clone())
                    .ok()
                    .is_some_and(|s| self.reference_ok(&s))
            {
                self.hidden += 1;
            }
        }
        syn::visit::visit_macro(self, node);
    }
    fn visit_attribute(&mut self, node: &'ast syn::Attribute) {
        if node.path().is_ident("path") {
            let valid = match &node.meta {
                syn::Meta::NameValue(value) => match &value.value {
                    syn::Expr::Lit(expr) => match &expr.lit {
                        syn::Lit::Str(path) => self.reference_ok(path),
                        _ => false,
                    },
                    _ => false,
                },
                _ => false,
            };
            if !valid {
                self.hidden += 1;
            }
        }
        use quote::ToTokens;
        fn names(stream: TokenStream, out: &mut Vec<String>) {
            for token in stream {
                match token {
                    TokenTree::Ident(id) => out.push(id.to_string()),
                    TokenTree::Group(g) => names(g.stream(), out),
                    _ => {}
                }
            }
        }
        let mut ids = vec![];
        names(node.meta.to_token_stream(), &mut ids);
        if ids.iter().any(|s| s == "unsafe_code")
            && ids
                .iter()
                .any(|s| matches!(s.as_str(), "allow" | "warn" | "expect"))
        {
            self.escapes += 1;
        }
        self.abi += ids
            .iter()
            .filter(|s| matches!(s.as_str(), "no_mangle" | "export_name" | "link_section"))
            .count();
        syn::visit::visit_attribute(self, node);
    }
    fn visit_expr_unsafe(&mut self, node: &'ast syn::ExprUnsafe) {
        self.blocks += 1;
        syn::visit::visit_expr_unsafe(self, node);
    }
    fn visit_signature(&mut self, node: &'ast syn::Signature) {
        if node.unsafety.is_some() {
            self.contracts += 1;
        }
        syn::visit::visit_signature(self, node);
    }
    fn visit_item_impl(&mut self, node: &'ast syn::ItemImpl) {
        if node.unsafety.is_some() {
            self.contracts += 1;
        }
        syn::visit::visit_item_impl(self, node);
    }
    fn visit_item_trait(&mut self, node: &'ast syn::ItemTrait) {
        if node.unsafety.is_some() {
            self.contracts += 1;
        }
        syn::visit::visit_item_trait(self, node);
    }
    fn visit_item_foreign_mod(&mut self, _: &'ast syn::ItemForeignMod) {
        self.contracts += 1;
    }
}
fn scan(path: &Path, root: &Path) {
    for entry in std::fs::read_dir(path).unwrap() {
        let entry = entry.unwrap();
        let path = entry.path();
        if entry.file_type().unwrap().is_dir() {
            scan(&path, root);
        } else if path.extension().is_some_and(|e| e == "rs") {
            let relative = path.strip_prefix(root).unwrap().to_str().unwrap();
            let source = std::fs::read_to_string(&path).unwrap();
            let tokens = unsafe_tokens(source.parse::<TokenStream>().unwrap());
            let file = syn::parse_file(&source).unwrap();
            let mut guard = Guard {
                location: Some((root.canonicalize().unwrap(), path.canonicalize().unwrap())),
                ..Guard::default()
            };
            guard.visit_file(&file);
            assert_eq!(guard.hidden, 0, "unscanned Rust include/path: {relative}");
            if let Some((_, cap)) = ALLOWLIST.iter().find(|(p, _)| *p == relative) {
                let counts = [
                    guard.blocks,
                    guard.contracts,
                    tokens,
                    guard.escapes,
                    guard.abi,
                ];
                assert!(
                    counts.iter().zip(cap).all(|(count, cap)| count <= cap),
                    "unsafe budget exceeded: {relative}: {counts:?} > {cap:?}"
                );
                let lines: Vec<_> = source.lines().collect();
                let mut documented = 0;
                for (i, line) in lines.iter().enumerate() {
                    // Every allowed existing block is written as unsafe { on
                    // one line; signature types and unsafe impls aren't blocks.
                    if line.contains("unsafe {") {
                        let mut prior = i;
                        let mut safety = false;
                        while prior > 0 {
                            prior -= 1;
                            let Some(line) = lines.get(prior).map(|s| s.trim()) else {
                                break;
                            };
                            if !line.starts_with("//") {
                                break;
                            }
                            if line.starts_with("// SAFETY:") {
                                safety = true;
                            }
                        }
                        assert!(
                            safety,
                            "missing adjacent SAFETY comment: {relative}:{}",
                            i + 1
                        );
                        documented += 1;
                    }
                }
                assert_eq!(
                    documented, guard.blocks,
                    "unsafe block syntax must remain reviewable: {relative}"
                );
            } else {
                assert_eq!(
                    (
                        guard.blocks,
                        guard.contracts,
                        tokens,
                        guard.escapes,
                        guard.abi
                    ),
                    (0, 0, 0, 0, 0),
                    "unsafe Rust outside allowlist: {relative}"
                );
            }
        }
    }
}
#[test]
fn unsafe_is_confined_to_documented_budgeted_paths() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    for dir in ["bins", "crates", "tools"] {
        scan(&root.join(dir), &root);
    }
    let cargo = std::fs::read_to_string(root.join("Cargo.toml")).unwrap();
    assert!(cargo.contains("unsafe_code = \"forbid\""));
}
#[test]
fn vectors_are_available_on_every_connection_after_one_registration() {
    embedding_worker::sqlite_vec_loader::register().unwrap();
    embedding_worker::sqlite_vec_loader::register().unwrap();
    for _ in 0..2 {
        let db = rusqlite::Connection::open_in_memory().unwrap();
        let v: String = db
            .query_row("SELECT vec_version()", [], |r| r.get(0))
            .unwrap();
        assert_eq!(v, "v0.1.9");
        db.execute_batch("CREATE VIRTUAL TABLE test_vec USING vec0(chunk_id INTEGER PRIMARY KEY,embedding float[2])").unwrap();
    }
}

fn policy_accepts(path: &str, source: &str) -> bool {
    let mut guard = Guard::default();
    guard.visit_file(&syn::parse_file(source).unwrap());
    let counts = [
        guard.blocks,
        guard.contracts,
        unsafe_tokens(source.parse().unwrap()),
        guard.escapes,
        guard.abi,
    ];
    if guard.hidden > 0 {
        return false;
    }
    let budget = ALLOWLIST
        .iter()
        .find(|(p, _)| *p == path)
        .map(|(_, b)| *b)
        .unwrap_or([0; 5]);
    counts.iter().zip(budget).all(|(count, cap)| *count <= cap)
}
#[test]
fn regression_p3_allowlisted_file_cannot_add_unsafe_contracts() {
    assert!(!policy_accepts(
        "crates/mem-return/src/lib.rs",
        "struct X; unsafe impl Send for X {}"
    ));
}
#[test]
fn regression_p3_lint_escape_and_abi_attributes_are_not_unrestricted() {
    for source in [
        "#[allow(unsafe_code)] fn escape() {}",
        "#[no_mangle] pub fn exported() {}",
        "#[export_name=\"other\"] fn renamed() {}",
        "#[link_section=\".init\"] static V:u8=0;",
    ] {
        assert!(!policy_accepts("crates/not-allowlisted/src/lib.rs", source));
    }
}

#[test]
fn regression_p3_non_rust_includes_and_path_attributes_are_rejected() {
    assert!(!policy_accepts(
        "crates/mem-return/src/lib.rs",
        "include!(\"hidden.fixture\");"
    ));
    assert!(!policy_accepts(
        "crates/mem-return/src/lib.rs",
        "#[path=\"hidden.fixture\"] mod hidden;"
    ));
}
