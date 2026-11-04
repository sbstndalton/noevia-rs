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
// Review-visible path and unsafe-block budgets. No hashes: harmless edits pass.
const ALLOWLIST: &[(&str, usize)] = &[
    ("crates/mem-return/src/lib.rs", 1),
    ("crates/gguf-memtest/tests/hostile.rs", 2),
    ("crates/embedding-worker/src/sqlite_vec_loader.rs", 2),
];
#[derive(Default)]
struct Guard {
    blocks: usize,
    contracts: usize,
}
impl<'ast> Visit<'ast> for Guard {
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
            let mut guard = Guard::default();
            guard.visit_file(&file);
            if let Some((_, cap)) = ALLOWLIST.iter().find(|(p, _)| *p == relative) {
                assert!(
                    guard.blocks <= *cap,
                    "unsafe block budget exceeded: {relative}"
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
                    (guard.blocks, guard.contracts, tokens),
                    (0, 0, 0),
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
    assert!(cargo.contains("unsafe_code = \"deny\""));
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
