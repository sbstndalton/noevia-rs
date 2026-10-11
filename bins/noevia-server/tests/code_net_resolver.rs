#![allow(clippy::unwrap_used)]
use noevia_server::code_net_resolver::{Fault, Resolver, Snapshot, Spec};
#[test]
fn literals_and_disabled_need_no_helper_or_dns() {
    let r = Resolver::new(Spec::parse("127.0.0.2 ::ffff:127.0.0.2").unwrap()).unwrap();
    assert_eq!(
        r.snapshot().unwrap().refuses(
            Some("::ffff:7f00:2".parse().unwrap()),
            std::time::Instant::now()
        ),
        Some(true)
    );
    assert!(matches!(
        Resolver::new(Spec::parse("127.0.0.3").unwrap()),
        Err(Fault::Conflict)
    ));
    r.shutdown().unwrap();
    let r = Resolver::new(Spec::parse("").unwrap()).unwrap();
    assert!(matches!(&*r.snapshot().unwrap(), Snapshot::Disabled));
    r.shutdown().unwrap();
}
