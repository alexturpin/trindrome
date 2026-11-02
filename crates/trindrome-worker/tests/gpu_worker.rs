//! The worker driving the CUDA backend end to end. Skips without a GPU unless
//! `TRINDROME_REQUIRE_GPU` is set.

use std::sync::atomic::AtomicBool;

use num_bigint::BigUint;
use trindrome_worker::configuration::{Config, Search, SearchMode};
use trindrome_worker::engine::Engine;
use trindrome_worker::persistence::Store;
use trindrome_worker::scheduler::{self, RunOptions};

#[test]
fn nvml_reads_the_cuda_device() {
    let backend = match trindrome_cuda::CudaBackend::new(0) {
        Ok(b) => b,
        Err(e) if std::env::var_os("TRINDROME_REQUIRE_GPU").is_some() => panic!("GPU required: {e}"),
        Err(e) => return eprintln!("skipping GPU test: {e}"),
    };
    let monitor = trindrome_worker::gpu_monitor::GpuMonitor::new(backend.pci_bus_id()).unwrap();
    let reading = monitor.read().unwrap();
    eprintln!("{} at {}: {reading:?}", backend.device_name(), backend.pci_bus_id());
    assert!((1..110).contains(&reading.temperature_c));
    assert!(reading.utilization_percent <= 100);
}

#[test]
fn gpu_worker_completes_a_mixed_plan_with_audits() {
    let dir = tempfile::tempdir().unwrap();
    let mut config = Config::default();
    config.storage.database_path = dir.path().join("search.sqlite");
    // Lengths 14-15 are below the GPU filter's minimum and run inline on the CPU.
    config.search = Search {
        mode: SearchMode::HistoricalReplay,
        minimum_hex_digits: 14,
        maximum_hex_digits: Some(28),
        stop_on_first_verified_discovery: true,
    };
    config.compute.chunk_candidates = 1 << 26;
    config.compute.audit_every_batches = 2;
    config.compute.audit_candidates = 2000;
    config.idle.require_ac_power = false;

    let engine = match Engine::cuda(&config.compute) {
        Ok(e) => e,
        Err(e) if std::env::var_os("TRINDROME_REQUIRE_GPU").is_some() => panic!("GPU required: {e}"),
        Err(e) => return eprintln!("skipping GPU test: {e}"),
    };
    let reason = scheduler::run(
        &config,
        vec![engine],
        &RunOptions {
            immediate: true,
            ..Default::default()
        },
        &AtomicBool::new(false),
    )
    .unwrap();
    assert_eq!(reason, "search plan exhausted");

    let store = Store::open(&config.storage.database_path).unwrap();
    assert_eq!(store.verify_integrity().unwrap(), Vec::<String>::new());
    let coverage = store.coverage().unwrap();
    assert!(coverage.iter().all(|c| c.covered == c.total));
    let planned: BigUint = coverage.iter().map(|c| c.total.clone()).sum();
    assert_eq!(BigUint::from(store.totals().unwrap().candidates), planned);

    let backends = store.coverage_by_backend().unwrap();
    assert!(
        backends.iter().any(|(b, _)| b.starts_with("cpu-reference")),
        "{backends:?}"
    );
    assert!(backends.iter().any(|(b, _)| b.starts_with("cuda")), "{backends:?}");
    let (audits, audited) = store.audit_summary().unwrap();
    assert!(audits >= 3 && audited >= 3 * 2000, "{audits} audits");
    assert!(store.discoveries().unwrap().is_empty());
}
