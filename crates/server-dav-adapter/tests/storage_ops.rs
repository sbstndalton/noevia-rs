use server_dav::Fault;
use server_dav_adapter::storage_ops::StorageOps;
use std::sync::{
    atomic::{AtomicUsize, Ordering},
    Arc,
};

#[test]
fn construction_and_each_mutation_refuse_without_sending() {
    let sends = Arc::new(AtomicUsize::new(0));
    let counted = Arc::clone(&sends);
    let fake_transport = move || {
        counted.fetch_add(1, Ordering::SeqCst);
    };

    let ops = StorageOps::new(fake_transport);
    assert_eq!(sends.load(Ordering::SeqCst), 0);

    for result in [
        ops.put(),
        ops.mkcol(),
        ops.delete(),
        ops.move_resource(),
        ops.copy(),
        ops.preserve(),
    ] {
        assert_eq!(result, Err(Fault::Unavailable));
    }

    assert_eq!(sends.load(Ordering::SeqCst), 0);
}
