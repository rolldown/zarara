#![cfg_attr(not(test), allow(dead_code, unused_imports))]

use output_fuzz_common::{acyclic_graph_case_strategy, run_deterministic_check};
use proptest::test_runner::{
    Config as ProptestConfig, RngSeed, TestCaseError, TestError, TestRunner,
};

#[test]
fn deterministic_output() {
    let mut config = ProptestConfig {
        failure_persistence: None,
        ..ProptestConfig::default()
    };
    if let Ok(seed_text) = std::env::var("PROPTEST_RNG_SEED") {
        let seed = seed_text
            .parse::<u64>()
            .expect("PROPTEST_RNG_SEED must be a u64");
        config.rng_seed = RngSeed::Fixed(seed);
    }

    let mut runner = TestRunner::new(config);
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();

    match runner.run(&acyclic_graph_case_strategy(), |case| {
        runtime
            .block_on(run_deterministic_check(case))
            .map_err(TestCaseError::fail)?;
        Ok(())
    }) {
        Ok(()) => {}
        Err(TestError::Fail(why, _)) => panic!("{why}"),
        Err(TestError::Abort(why)) => panic!("Proptest aborted: {why}"),
    }
}
