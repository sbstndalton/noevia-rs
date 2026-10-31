//! The research store stays compiled for the front's own jobs (research_routes.rs).
#[test]
fn compiled_server_advertises_live_research_store() {
    assert!(noevia_server::FEATURES
        .contains(&server_projects::research_jobs::RESEARCH_JOBS_STORE_MARKER));
}
