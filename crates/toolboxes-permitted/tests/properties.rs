//! Properties of the toolboxes-permitted port: no input panics; a connector box is carried only
//! when the account connected it; an unavailable box is never active and none of its tools is
//! permitted; a blocked tool is always unavailable and a write always needs approval at least; the
//! coding harness is available only to an administrator in a Cowork session with a project, the
//! harness on and a repository. The JS side of "never more permissive than the JS" runs against
//! the real JS in noevia-core's differential test.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use prompt_framing::js::units;
use proptest::prelude::*;
use std::collections::HashSet;
use toolboxes_permitted::{
    call, permitted, selected_ids, Box as ToolBox, Input, Permission, Policy, Project, Tool, Work,
};

fn work() -> Work {
    Work::new(1 << 30)
}

const IDS: [&str; 6] = ["core", "web", "diary", "gmail", "sso", "project-docs"];

fn id() -> impl Strategy<Value = Vec<u16>> {
    prop::sample::select(IDS.to_vec()).prop_map(units)
}

fn ids() -> impl Strategy<Value = Vec<Option<Vec<u16>>>> {
    prop::collection::vec(prop::option::weighted(0.9, id()), 0..5)
}

fn project() -> impl Strategy<Value = Option<Project>> {
    prop::option::of(
        (any::<bool>(), any::<bool>(), prop::option::of(ids())).prop_map(
            |(auto, docs_defaulted, toolboxes)| Project {
                auto,
                docs_defaulted,
                toolboxes,
            },
        ),
    )
}

fn set() -> impl Strategy<Value = HashSet<Vec<u16>>> {
    prop::collection::hash_set(id(), 0..3)
}

fn input() -> impl Strategy<Value = Input> {
    let tool = (any::<bool>(), 0u8..3).prop_map(|(write, p)| Tool {
        write,
        policy: [Policy::Block, Policy::Ask, Policy::Other][usize::from(p)],
    });
    let boxes = prop::collection::vec(
        (id(), any::<bool>(), prop::collection::vec(tool, 0..4)).prop_map(|(id, ready, tools)| {
            ToolBox {
                id,
                ready: Some(ready),
                tools,
            }
        }),
        0..6,
    );
    (
        (
            any::<bool>(),
            project(),
            any::<bool>(),
            ids(),
            set(),
            ids(),
            set(),
        ),
        (
            any::<bool>(),
            any::<bool>(),
            any::<bool>(),
            boxes,
            prop::collection::vec(prop::option::of(prop::option::of(id())), 0..4),
        ),
    )
        .prop_map(
            |(
                (is_admin, project, cowork, defaults, connector, connected, oauth),
                (diary_enabled, harness_enabled, has_repositories, boxes, manifest),
            )| Input {
                is_admin,
                project,
                cowork,
                defaults,
                docs: units("project-docs"),
                connector,
                connected,
                oauth,
                diary_enabled,
                harness_enabled,
                has_repositories,
                boxes,
                manifest,
            },
        )
}

proptest! {
    #![proptest_config(ProptestConfig { cases: 2000, ..ProptestConfig::default() })]

    #[test]
    fn random_bytes_never_panic(op in 0u8..5, body in prop::collection::vec(any::<u8>(), 0..300)) {
        let mut input = vec![op];
        input.extend(body);
        let (status, reply) = call(&input);
        prop_assert!(status <= 1);
        prop_assert!(!reply.is_empty());
    }

    #[test]
    fn connectors_only_when_connected(i in input()) {
        let carried = selected_ids(i.project.as_ref(), &i.defaults, &i.docs, &i.connector, &i.connected, &mut work()).unwrap();
        for id in carried.iter().flatten() {
            if i.connector.contains(id) {
                prop_assert!(i.connected.iter().any(|c| c.as_ref() == Some(id)));
            }
        }
    }

    #[test]
    fn the_catalogue_never_permits_what_it_must_not(i in input()) {
        let out = permitted(&i, &mut work()).unwrap();
        prop_assert!(out.len() > i.boxes.len());
        for (k, b) in out.iter().enumerate() {
            if !b.available {
                prop_assert!(!b.active);
                prop_assert!(b.tools.iter().all(|t| t.permission == Permission::Unavailable));
            }
            if let Some(src) = i.boxes.get(k) {
                if i.connector.contains(&src.id) && !i.connected.iter().any(|c| c.as_ref() == Some(&src.id)) {
                    prop_assert!(!b.available);
                }
                for (t, s) in b.tools.iter().zip(&src.tools) {
                    if s.policy == Policy::Block { prop_assert_eq!(t.permission, Permission::Unavailable); }
                    if s.write { prop_assert!(t.permission != Permission::Allowed); }
                }
            }
        }
        let code = out.last().unwrap();
        prop_assert_eq!(code.available, i.cowork && i.is_admin && i.harness_enabled && i.project.is_some() && i.has_repositories);
        prop_assert_eq!(code.tools[1].permission == Permission::Allowed, false);
        prop_assert_eq!(code.tools[2].permission == Permission::Allowed, false);
    }
}
