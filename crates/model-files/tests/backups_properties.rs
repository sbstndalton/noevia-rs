//! Properties of the backup plan (noevia#1021): only listed copies of the right file are ever
//! removed, this write's copy never is, retention is bounded, and a hinted write makes nothing.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing)]

use model_files::backups::{
    is_revision_copy, is_rotating_copy, plan, plan_json, Listed, Request, KEEP_ROTATING,
};
use proptest::prelude::*;

fn name() -> impl Strategy<Value = String> {
    prop_oneof![
        "[0-9a-f]{64}".prop_map(|h| format!("models.ini.noevia-backup-{h}")),
        "[0-9a-f]{2}".prop_map(|h| format!("models.ini.noevia-backup-{}", h.repeat(32))),
        "[0-9a-z-]{1,12}".prop_map(|t| format!("models.ini.bak-{t}")),
        "[a-z.]{1,20}",
        Just("models.ini".to_owned()),
    ]
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(512))]

    #[test]
    fn any_bytes_never_panic(b in proptest::collection::vec(any::<u8>(), 0..400)) {
        let _ = plan_json(&b);
    }

    #[test]
    fn plans_are_safe_and_bounded(
        names in proptest::collection::btree_set(name(), 0..40),
        mtimes in proptest::collection::vec(0u128..20, 40),
        base in proptest::option::of("[0-9a-f]{64}"),
        backup in any::<bool>(),
        rot in proptest::option::of("[0-9]{1,8}"),
        keep in 1usize..15,
    ) {
        let existing: Vec<Listed> = names.iter().zip(&mtimes).map(|(n, &m)| Listed { name: n.clone(), mtime_ns: m }).collect();
        let r = Request { file: "models.ini".into(), base_revision: base, backup,
            rotating_name: rot.map(|t| format!("models.ini.bak-{t}")), keep_revisions: keep, existing: existing.clone() };
        let p = plan(&r);
        for n in &p.prune {
            prop_assert!(existing.iter().any(|l| &l.name == n), "pruned a name that was not listed");
            prop_assert!(is_revision_copy("models.ini", n) || is_rotating_copy("models.ini", n), "pruned a foreign name {}", n);
            prop_assert!(Some(n) != p.revision.as_ref() && Some(n) != p.rotating.as_ref());
        }
        if !backup { prop_assert!(p.rotating.is_none() && p.revision.is_none()); }
        let left = |f: fn(&str, &str) -> bool| existing.iter().filter(|l| f("models.ini", &l.name) && !p.prune.contains(&l.name)).count();
        let new_rev = usize::from(p.revision.as_ref().is_some_and(|n| !existing.iter().any(|l| &l.name == n)));
        prop_assert!(left(is_revision_copy) + new_rev <= keep);
        prop_assert!(left(is_rotating_copy) + usize::from(p.rotating.is_some()) <= KEEP_ROTATING + 1);
        // Some revision copy always survives when one existed or is made.
        if existing.iter().any(|l| is_revision_copy("models.ini", &l.name)) || p.revision.is_some() {
            prop_assert!(left(is_revision_copy) + new_rev >= 1);
        }
        prop_assert_eq!(plan(&r), p);
    }
}
