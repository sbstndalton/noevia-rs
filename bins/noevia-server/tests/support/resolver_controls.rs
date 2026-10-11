#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::panic
)]
use super::*;
fn fixture() -> std::path::PathBuf {
    let path = std::path::PathBuf::from("/tmp/m6-resolver-fixture-v1");
    assert!(
        path.is_file(),
        "compile closed fixture before focused tests"
    );
    path
}
fn finish(resolver: &Resolver) {
    resolver.request_shutdown();
    let limit = Instant::now() + Duration::from_secs(2);
    loop {
        match resolver.shutdown() {
            Ok(()) => break,
            Err(Fault::Busy) if Instant::now() < limit => thread::sleep(Duration::from_millis(5)),
            Err(Fault::Cleanup) if !resolver.leased() => {
                // Test-only teardown of a permanent production quarantine, after
                // positive controller join and child reap. No recovery API exists.
                assert!(resolver.owner.child.try_lock().unwrap().is_none());
                assert!(resolver.owner.handle.try_lock().unwrap().is_none());
                assert!(GLOBAL.try_lock().unwrap().is_some());
                resolver.owner.quarantine.store(false, Ordering::Release);
            }
            other => panic!("shutdown {other:?}"),
        }
    }
}
fn frame(generation: u64, address: &str) -> Vec<u8> {
    let mut out = b"NA01".to_vec();
    out.extend_from_slice(&generation.to_be_bytes());
    out.extend_from_slice(&[1, 0, 0, 0, 1]);
    out.push(address.len() as u8);
    out.extend_from_slice(address.as_bytes());
    out
}
#[test]
fn bounds_protocol_cache_properties() {
    assert!(Spec::parse(&" ".repeat(8192)).is_ok());
    assert_eq!(Spec::parse(&" ".repeat(8193)), Err(Fault::Cap));
    assert!(Spec::parse(&vec!["egress"; 32].join(",")).is_ok());
    assert_eq!(Spec::parse(&vec!["egress"; 33].join(",")), Err(Fault::Cap));
    for raw in [
        "fe80::1%eth0",
        "xn--example",
        "egréss",
        "a.1",
        "010.0.0.1",
        "a_b",
    ] {
        assert!(Spec::parse(raw).is_err());
    }
    let spec = Spec::parse("egress").unwrap();
    let valid = frame(7, "127.0.0.2");
    assert!(response(&valid, &spec, 7).is_ok());
    assert_eq!(response(&valid, &spec, 8), Err(Fault::Protocol));
    for n in 0..valid.len() {
        assert!(response(&valid[..n], &spec, 7).is_err());
    }
    let mut extra = valid.clone();
    extra.push(0);
    assert!(response(&extra, &spec, 7).is_err());
    let mut union = BTreeSet::new();
    for n in 0..256 {
        merge(
            &mut union,
            BTreeSet::from([IpAddr::V4(std::net::Ipv4Addr::new(10, 0, 0, n as u8))]),
        )
        .unwrap();
    }
    let saved = union.clone();
    assert_eq!(
        merge(&mut union, BTreeSet::from(["11.0.0.1".parse().unwrap()])),
        Err(Fault::Cap)
    );
    assert!(saved.iter().all(|a| union.contains(a)));
    let now = Instant::now();
    let ready = Snapshot::Ready {
        generation: 7,
        expires: Some(now),
        deny_union: union,
    };
    assert_eq!(ready.refuses(Some("12.0.0.1".parse().unwrap()), now), None);
    for address in saved {
        assert_eq!(ready.refuses(Some(address), now), Some(true));
    }
    assert_eq!(
        Snapshot::Pending {
            deny_union: BTreeSet::new()
        }
        .refuses(Some("127.0.0.1".parse().unwrap()), now),
        None
    );
    assert_eq!(
        Snapshot::Quarantined.refuses(Some("127.0.0.1".parse().unwrap()), now),
        Some(true)
    );
    let mut wait = Duration::from_secs(5);
    for expected in [10, 20, 40, 80, 160, 300, 300] {
        wait = backoff(wait);
        assert_eq!(wait.as_secs(), expected);
    }
}
#[test]
fn global_publication_and_positive_reap() {
    let r = Resolver::construct(
        Spec::parse("egress").unwrap(),
        Executable::Fixture(fixture()),
    )
    .unwrap();
    let borrowed = Resolver::construct(
        Spec::parse("egress").unwrap(),
        Executable::Fixture(fixture()),
    )
    .unwrap();
    assert!(Arc::ptr_eq(&r.owner, &borrowed.owner));
    assert!(matches!(
        Resolver::construct(
            Spec::parse("other").unwrap(),
            Executable::Fixture(fixture())
        ),
        Err(Fault::Conflict)
    ));
    let limit = Instant::now() + Duration::from_secs(2);
    while !matches!(&*r.snapshot().unwrap(), Snapshot::Ready { .. }) && Instant::now() < limit {
        thread::sleep(Duration::from_millis(5));
    }
    assert_eq!(
        r.snapshot()
            .unwrap()
            .refuses(Some("127.0.0.2".parse().unwrap()), Instant::now()),
        Some(true)
    );
    assert!(!r.leased());
    assert!(r.owner.child.try_lock().unwrap().is_none());
    let lock = r.owner.snapshot.try_lock().unwrap();
    assert!(matches!(r.snapshot(), Err(Fault::Busy)));
    drop(lock);
    finish(&r);
}
#[test]
fn cancellation_retains_capacity_until_join() {
    let r = Resolver::construct(
        Spec::parse("blocked").unwrap(),
        Executable::Fixture(fixture()),
    )
    .unwrap();
    let limit = Instant::now() + Duration::from_secs(2);
    while !r.leased() && Instant::now() < limit {
        thread::sleep(Duration::from_millis(5));
    }
    assert!(r.leased());
    let registry = GLOBAL.try_lock().unwrap();
    r.request_shutdown();
    assert!(matches!(r.shutdown(), Err(Fault::Busy)));
    assert!(registry.is_some());
    drop(registry);
    finish(&r);
    assert!(GLOBAL.try_lock().unwrap().is_none());
}
#[test]
fn closed_process_protocol_failures() {
    for host in [
        "stale",
        "suffix",
        "truncated",
        "flood",
        "stderr",
        "exit",
        "records",
    ] {
        let r = Resolver::construct(Spec::parse(host).unwrap(), Executable::Fixture(fixture()))
            .unwrap();
        let limit = Instant::now() + Duration::from_secs(2);
        while !r.owner.quarantine.load(Ordering::Acquire) && Instant::now() < limit {
            thread::sleep(Duration::from_millis(5));
        }
        assert!(!matches!(&*r.snapshot().unwrap(), Snapshot::Ready { .. }));
        finish(&r);
    }
    let r = Resolver::construct(
        Spec::parse("egress").unwrap(),
        Executable::Fixture("/tmp/m6-resolver-fixture-absent".into()),
    )
    .unwrap();
    thread::sleep(Duration::from_millis(30));
    assert!(!r.leased());
    finish(&r);
}
#[test]
fn whole_attempt_deadline_reaps_blocked_helper() {
    let r = Resolver::construct(
        Spec::parse("blocked").unwrap(),
        Executable::Fixture(fixture()),
    )
    .unwrap();
    let start = Instant::now();
    let limit = start + Duration::from_secs(7);
    while r.generation() == 0 || !r.leased() {
        assert!(Instant::now() < limit);
        thread::sleep(Duration::from_millis(5));
    }
    assert!(matches!(
        r.owner.child.try_lock(),
        Err(std::sync::TryLockError::WouldBlock)
    ));
    while r.leased() {
        assert!(Instant::now() < limit);
        thread::sleep(Duration::from_millis(5));
    }
    assert!(Instant::now().duration_since(start) >= Duration::from_secs(4));
    assert!(matches!(&*r.snapshot().unwrap(), Snapshot::Quarantined));
    assert!(r.owner.child.try_lock().unwrap().is_none());
    finish(&r);
}
#[test]
fn selected_stale_generation_assertion() {
    let spec = Spec::parse("egress").unwrap();
    assert_eq!(
        response(&frame(7, "127.0.0.2"), &spec, 8),
        Err(Fault::Protocol)
    );
}
#[test]
fn selected_lifetime_union_assertion() {
    let mut union = BTreeSet::from(["127.0.0.2".parse().unwrap()]);
    merge(&mut union, BTreeSet::from(["127.0.0.3".parse().unwrap()])).unwrap();
    assert_eq!(union.len(), 2);
}
#[test]
fn selected_expiry_assertion() {
    let now = Instant::now();
    let ready = Snapshot::Ready {
        generation: 7,
        expires: Some(now),
        deny_union: BTreeSet::new(),
    };
    assert_eq!(ready.refuses(Some("127.0.0.1".parse().unwrap()), now), None);
}
#[test]
#[allow(clippy::bool_assert_comparison)]
fn selected_global_configuration_assertion() {
    let r = Resolver::new(Spec::parse("127.0.0.2").unwrap()).unwrap();
    assert_eq!(
        matches!(
            Resolver::new(Spec::parse("127.0.0.3").unwrap()),
            Err(Fault::Conflict)
        ),
        true
    );
    finish(&r);
}
#[test]
fn selected_reap_guard_assertion() {
    let r = Resolver::new(Spec::parse("").unwrap()).unwrap();
    r.owner.lease.store(true, Ordering::Release);
    assert_eq!(r.shutdown(), Err(Fault::Cleanup));
    r.owner.lease.store(false, Ordering::Release);
    finish(&r);
}
#[test]
fn finite_protocol_caps_and_node_refusal_property() {
    let literal_text = (0..256)
        .map(|n| format!("10.0.0.{n}"))
        .collect::<Vec<_>>()
        .join(",");
    let parsed = Spec::parse(&literal_text).unwrap();
    assert_eq!(parsed.literals.len(), 256);
    assert_eq!(
        Spec::parse(&(literal_text.clone() + ",11.0.0.1")),
        Err(Fault::Cap)
    );
    let long = "a".repeat(253);
    assert!(Spec::parse(&long).is_ok());
    assert_eq!(Spec::parse(&(long + "a")), Err(Fault::Cap));
    let hosts = Spec::parse(&vec!["egress"; 32].join(",")).unwrap();
    let req = request(&hosts, 1).unwrap();
    assert_eq!(parse_request(&req).unwrap().1, hosts);
    for n in 0..req.len() {
        assert!(parse_request(&req[..n]).is_err());
    }
    let mut req_extra = req.clone();
    req_extra.push(0);
    assert!(parse_request(&req_extra).is_err());
    let spec = Spec::parse("egress").unwrap();
    let mut invalid = frame(1, "127.0.0.2");
    invalid[13] = 1;
    assert_eq!(response(&invalid, &spec, 1), Err(Fault::Protocol));
    let mut capped = frame(1, "127.0.0.2");
    capped[15] = 1;
    capped[16] = 1;
    assert_eq!(response(&capped, &spec, 1), Err(Fault::Cap));
    assert_eq!(response(&vec![0; 32769], &spec, 1), Err(Fault::Cap));
    assert!(parse_request(&vec![0; 8193]).is_err());
    let addresses = ["127.0.0.2", "0:0::1", "::ffff:7f00:3"];
    let units = addresses
        .iter()
        .map(|s| s.encode_utf16().collect::<Vec<_>>())
        .collect::<Vec<_>>();
    let refs = units.iter().map(Vec::as_slice).collect::<Vec<_>>();
    let deny_union = addresses.iter().map(|s| ip(s).unwrap()).collect();
    let snapshot = Snapshot::Ready {
        generation: 1,
        expires: None,
        deny_union,
    };
    for local in [
        "127.0.0.2",
        "::ffff:127.0.0.2",
        "::ffff:7f00:2",
        "::1",
        "0:0:0:0:0:0:0:1",
        "127.0.0.3",
        "127.0.0.4",
    ] {
        let raw = local.encode_utf16().collect::<Vec<_>>();
        let denied = code_net_guard::refuses(&refs, Some(&raw)).unwrap();
        if denied {
            assert_eq!(
                snapshot.refuses(Some(local.parse().unwrap()), Instant::now()),
                Some(true)
            );
        }
    }
    let mut seed = 1u64;
    for size in 0..600 {
        let mut bytes = Vec::new();
        for _ in 0..size {
            seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1);
            bytes.push((seed >> 32) as u8);
        }
        let _ = parse_request(&bytes);
        let _ = response(&bytes, &spec, 1);
    }
}
#[test]
fn partial_generation_history_never_becomes_more_permissive() {
    let spec = Spec::parse("egress nx").unwrap();
    let mut partial = b"NA01".to_vec();
    partial.extend_from_slice(&1u64.to_be_bytes());
    partial.extend_from_slice(&[2, 0, 0, 0, 1, 9]);
    partial.extend_from_slice(b"127.0.0.2");
    partial.extend_from_slice(&[1, 1, 0, 0]);
    let candidate = response(&partial, &spec, 1).unwrap();
    assert!(!candidate.complete);
    let mut union = BTreeSet::new();
    merge(&mut union, candidate.addresses).unwrap();
    let pending = Snapshot::Pending {
        deny_union: union.clone(),
    };
    assert_eq!(
        pending.refuses(Some("127.0.0.2".parse().unwrap()), Instant::now()),
        Some(true)
    );
    assert_eq!(pending.refuses(None, Instant::now()), Some(true));
    let mut success = b"NA01".to_vec();
    success.extend_from_slice(&2u64.to_be_bytes());
    success.extend_from_slice(&[2, 0, 0, 0, 1, 9]);
    success.extend_from_slice(b"127.0.0.3");
    success.extend_from_slice(&[1, 0, 0, 1, 9]);
    success.extend_from_slice(b"127.0.0.4");
    let candidate = response(&success, &spec, 2).unwrap();
    assert!(candidate.complete);
    merge(&mut union, candidate.addresses).unwrap();
    assert_eq!(union.len(), 3);
    let ready = Snapshot::Ready {
        generation: 2,
        expires: Some(Instant::now() + TTL),
        deny_union: union,
    };
    for address in ["127.0.0.2", "127.0.0.3", "127.0.0.4"] {
        assert_eq!(
            ready.refuses(Some(address.parse().unwrap()), Instant::now()),
            Some(true)
        );
    }
}
#[allow(clippy::panic)]
pub(super) fn injected_controller_panic() {
    panic!("closed controller panic fixture")
}
#[test]
fn controller_panic_keeps_child_and_global_quarantine() {
    let r = Resolver::construct(
        Spec::parse("egress").unwrap(),
        Executable::PanicFixture(fixture()),
    )
    .unwrap();
    let limit = Instant::now() + Duration::from_secs(2);
    while !r.owner.quarantine.load(Ordering::Acquire) {
        assert!(Instant::now() < limit);
        thread::sleep(Duration::from_millis(5));
    }
    assert!(r.leased());
    let child = match r.owner.child.try_lock() {
        Err(std::sync::TryLockError::Poisoned(e)) => e.into_inner(),
        other => panic!("expected retained poisoned slot: {other:?}"),
    };
    assert!(child.is_some());
    drop(child);
    assert!(matches!(
        Resolver::new(Spec::parse("other").unwrap()),
        Err(Fault::Conflict)
    ));
    r.request_shutdown();
    loop {
        match r.shutdown() {
            Err(Fault::Busy) if Instant::now() < limit => thread::sleep(Duration::from_millis(5)),
            Err(Fault::Cleanup) => break,
            other => panic!("expected permanent quarantine {other:?}"),
        }
    }
    assert!(!r.leased());
    assert!(GLOBAL.try_lock().unwrap().is_some());
    assert!(r
        .owner
        .child
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .is_none());
    r.owner.quarantine.store(false, Ordering::Release);
    r.shutdown().unwrap();
}
#[test]
fn actual_partial_and_nx_frames_remain_unavailable() {
    let r = Resolver::construct(
        Spec::parse("partial nx").unwrap(),
        Executable::Fixture(fixture()),
    )
    .unwrap();
    let limit = Instant::now() + Duration::from_secs(2);
    loop {
        if r.snapshot()
            .unwrap()
            .refuses(Some("127.0.0.2".parse().unwrap()), Instant::now())
            == Some(true)
        {
            break;
        }
        assert!(Instant::now() < limit);
        thread::sleep(Duration::from_millis(5));
    }
    assert!(matches!(&*r.snapshot().unwrap(), Snapshot::Pending { .. }));
    assert_eq!(
        r.snapshot()
            .unwrap()
            .refuses(Some("127.0.0.3".parse().unwrap()), Instant::now()),
        None
    );
    finish(&r);
    let r =
        Resolver::construct(Spec::parse("nx").unwrap(), Executable::Fixture(fixture())).unwrap();
    thread::sleep(Duration::from_millis(50));
    assert!(matches!(&*r.snapshot().unwrap(), Snapshot::Pending { .. }));
    assert!(!r.owner.quarantine.load(Ordering::Acquire));
    finish(&r);
}
