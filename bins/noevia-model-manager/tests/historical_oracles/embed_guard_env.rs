// Frozen child-era environment mapping for #1668. The native-only service does not
// launch a Python child; retain the old mapping and assertions only as a test oracle.
/// #1668: what the Python child is told about the guarded embedding service: its container and
/// address, so it skips only env names that point at it. `None` clears them all.
fn embed_guard_env(service: Option<(&str, &str)>) -> [(&'static str, Option<String>); 3] {
    [
        (
            "NOEVIA_EMBED_SERVICE_GUARDED",
            service.map(|_| "1".to_owned()),
        ),
        (
            "NOEVIA_EMBED_SERVICE_CONTAINER",
            service.map(|(c, _)| c.to_owned()),
        ),
        (
            "NOEVIA_EMBED_SERVICE_URL",
            service.map(|(_, u)| u.to_owned()),
        ),
    ]
}

#[test]
fn the_python_child_learns_which_container_and_address_are_guarded() {
    let on = embed_guard_env(Some(("cowork-embed-1", "http://embed:8080")));
    assert_eq!(
        on.map(|(k, v)| (k, v.unwrap_or_default())),
        [
            ("NOEVIA_EMBED_SERVICE_GUARDED", "1".to_owned()),
            (
                "NOEVIA_EMBED_SERVICE_CONTAINER",
                "cowork-embed-1".to_owned()
            ),
            ("NOEVIA_EMBED_SERVICE_URL", "http://embed:8080".to_owned()),
        ]
    );
    assert!(embed_guard_env(None).iter().all(|(_, v)| v.is_none()));
}
