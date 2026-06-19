#![cfg_attr(not(test), allow(dead_code, unused_imports))]

mod graph_cycle_checker;

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as BASE64_STANDARD;
use flate2::Compression;
use flate2::write::ZlibEncoder;
use oxc::allocator::Allocator as OxcAllocator;
use oxc::parser::{ParseOptions, Parser};
use oxc::span::SourceType;
use proptest::prelude::*;
use proptest::test_runner::{
    Config as ProptestConfig, RngSeed, TestCaseError, TestError, TestRunner,
};
use rolldown::{
    Bundler, BundlerOptions, InputItem, IsExternal, OutputFormat, PreserveEntrySignatures,
    TreeshakeOptions,
};
use rolldown_common::Output;
use std::collections::{HashMap, HashSet};
use std::io::Write;
use std::path::{Component, Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

const MAX_NODES: usize = 12;
const MAX_EXTERNAL_MODULES: usize = 4;

fn external_module_name(index: usize) -> String {
    format!("external-{index}")
}

#[derive(Clone)]
struct GraphCase {
    seed: u64,
    node_count: usize,
    entry_nodes: Vec<usize>,
    cjs_nodes: Vec<usize>,
    static_edges: Vec<(usize, usize)>,
    dynamic_edges: Vec<(usize, usize)>,
    reexport_static_edges: Vec<(usize, usize)>,
    external_module_count: usize,
    external_dynamic_edges: Vec<(usize, usize)>,
    preserve_entry_signatures: PreserveEntrySignatures,
    strict_execution_order: bool,
    treeshake: bool,
    minify_internal_exports: bool,
}

impl std::fmt::Debug for GraphCase {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("GraphCase")
            .field("seed", &self.seed)
            .finish()
    }
}

#[test]
fn acyclic_input_produces_acyclic_output() {
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
            .block_on(run_case(case))
            .map_err(TestCaseError::fail)?;
        Ok(())
    }) {
        Ok(()) => {}
        Err(TestError::Fail(why, _)) => panic!("{why}"),
        Err(TestError::Abort(why)) => panic!("Proptest aborted: {why}"),
    }
}

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

/// Strategy that generates the topology known to trigger output cycles:
/// entries dynamically import a hub, the hub statically+dynamically imports children,
/// and the hub has external dynamic imports.
fn dynamic_hub_case_strategy() -> impl Strategy<Value = GraphCase> {
    (
        any::<u64>().no_shrink(),
        5usize..=MAX_NODES,
        0u8..4u8,
        any::<bool>(),
        any::<bool>(),
        any::<bool>(),
        1usize..=MAX_EXTERNAL_MODULES,
    )
        .prop_flat_map(
            move |(
                seed,
                node_count,
                preserve_entry_signatures_index,
                strict_execution_order,
                treeshake,
                minify_internal_exports,
                external_module_count,
            )| {
                // Pick how many entries (1..3) and how many children (2..min(4, remaining))
                let max_entries = (node_count - 2).min(3);
                let max_children = (node_count - 2).min(4);
                (
                    Just(seed),
                    Just(node_count),
                    Just(preserve_entry_signatures_index),
                    Just(strict_execution_order),
                    Just(treeshake),
                    Just(minify_internal_exports),
                    Just(external_module_count),
                    1usize..=max_entries,
                    2usize..=max_children,
                    prop::collection::vec(any::<bool>(), external_module_count),
                )
            },
        )
        .prop_map(
            |(
                seed,
                node_count,
                preserve_entry_signatures_index,
                strict_execution_order,
                treeshake,
                minify_internal_exports,
                external_module_count,
                num_entries,
                num_children,
                ext_mask,
            )| {
                // Layout: entries are 0..num_entries, hub is num_entries, children are (num_entries+1)..
                let hub = num_entries;
                let children_start = hub + 1;
                let children_end = (children_start + num_children).min(node_count);

                let entry_nodes = (0..num_entries).collect::<Vec<_>>();

                let mut static_edges = Vec::new();
                let mut dynamic_edges = Vec::new();

                // Entries only dynamically import the hub (no static path to hub/children)
                for &e in &entry_nodes {
                    dynamic_edges.push((e, hub));
                }

                // Hub statically imports all children
                for c in children_start..children_end {
                    static_edges.push((hub, c));
                    // Hub also dynamically imports children (dual import)
                    dynamic_edges.push((hub, c));
                }

                // Hub has external dynamic imports
                let mut external_dynamic_edges = Vec::new();
                for (ext, &has_edge) in ext_mask.iter().enumerate() {
                    if has_edge {
                        external_dynamic_edges.push((hub, ext));
                    }
                }
                // Ensure at least one external dynamic import from hub
                if external_dynamic_edges.is_empty() {
                    external_dynamic_edges.push((hub, 0));
                }

                let preserve_entry_signatures =
                    preserve_entry_signatures_from_index(preserve_entry_signatures_index);

                GraphCase {
                    seed,
                    node_count,
                    entry_nodes,
                    cjs_nodes: Vec::new(), // all ESM
                    static_edges,
                    dynamic_edges,
                    reexport_static_edges: Vec::new(),
                    external_module_count,
                    external_dynamic_edges,
                    preserve_entry_signatures,
                    strict_execution_order,
                    treeshake,
                    minify_internal_exports,
                }
            },
        )
}

#[test]
fn dynamic_hub_produces_acyclic_output() {
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

    match runner.run(&dynamic_hub_case_strategy(), |case| {
        runtime
            .block_on(run_case(case))
            .map_err(TestCaseError::fail)?;
        Ok(())
    }) {
        Ok(()) => {}
        Err(TestError::Fail(why, _)) => panic!("{why}"),
        Err(TestError::Abort(why)) => panic!("Proptest aborted: {why}"),
    }
}

fn acyclic_graph_case_strategy() -> impl Strategy<Value = GraphCase> {
    (
        any::<u64>().no_shrink(),
        3usize..=MAX_NODES,
        0u8..4u8,
        any::<bool>(),
        any::<bool>(),
        any::<bool>(),
        1usize..=MAX_EXTERNAL_MODULES,
    )
        .prop_flat_map(
            |(
                seed,
                node_count,
                preserve_entry_signatures_index,
                strict_execution_order,
                treeshake,
                minify_internal_exports,
                external_module_count,
            )| {
                let static_edge_slots = node_count * (node_count - 1) / 2;
                let dynamic_edge_slots = node_count * (node_count - 1);
                let external_dynamic_slots = node_count * external_module_count;
                (
                    Just(seed),
                    Just(node_count),
                    Just(preserve_entry_signatures_index),
                    Just(strict_execution_order),
                    Just(treeshake),
                    Just(minify_internal_exports),
                    Just(external_module_count),
                    prop::collection::vec(any::<bool>(), node_count),
                    prop::collection::vec(any::<u8>().prop_map(|x| x % 5 == 0), node_count),
                    prop::collection::vec(any::<bool>(), static_edge_slots),
                    (
                        prop::collection::vec(0u8..=2u8, dynamic_edge_slots),
                        prop::collection::vec(any::<bool>(), static_edge_slots),
                        prop::collection::vec(any::<bool>(), external_dynamic_slots),
                    ),
                )
            },
        )
        .prop_map(
            |(
                seed,
                node_count,
                preserve_entry_signatures_index,
                strict_execution_order,
                treeshake,
                minify_internal_exports,
                external_module_count,
                entry_mask,
                cjs_mask,
                static_mask,
                (dynamic_mask, reexport_mask, external_dynamic_mask),
            )| {
                build_case_from_masks(
                    seed,
                    node_count,
                    preserve_entry_signatures_index,
                    strict_execution_order,
                    treeshake,
                    minify_internal_exports,
                    &entry_mask,
                    &cjs_mask,
                    &static_mask,
                    &dynamic_mask,
                    &reexport_mask,
                    external_module_count,
                    &external_dynamic_mask,
                )
            },
        )
}

fn preserve_entry_signatures_from_index(index: u8) -> PreserveEntrySignatures {
    match index % 4 {
        0 => PreserveEntrySignatures::AllowExtension,
        1 => PreserveEntrySignatures::Strict,
        2 => PreserveEntrySignatures::ExportsOnly,
        _ => PreserveEntrySignatures::False,
    }
}

fn build_case_from_masks(
    seed: u64,
    node_count: usize,
    preserve_entry_signatures_index: u8,
    strict_execution_order: bool,
    treeshake: bool,
    minify_internal_exports: bool,
    entry_mask: &[bool],
    cjs_mask: &[bool],
    static_mask: &[bool],
    dynamic_mask: &[u8],
    reexport_mask: &[bool],
    external_module_count: usize,
    external_dynamic_mask: &[bool],
) -> GraphCase {
    let mut entry_nodes = entry_mask
        .iter()
        .enumerate()
        .filter_map(|(index, selected)| selected.then_some(index))
        .collect::<Vec<_>>();
    if entry_nodes.is_empty() {
        entry_nodes.push((seed as usize) % node_count);
    } else if entry_nodes.len() == node_count && node_count > 1 {
        entry_nodes.pop();
    }

    let mut static_edges = Vec::new();
    let mut dynamic_edges = Vec::new();
    let mut reexport_static_edges = Vec::new();
    let cjs_nodes = cjs_mask
        .iter()
        .enumerate()
        .filter_map(|(index, selected)| selected.then_some(index))
        .collect::<Vec<_>>();
    let preserve_entry_signatures =
        preserve_entry_signatures_from_index(preserve_entry_signatures_index);

    let mut idx = 0;
    for from in 0..node_count {
        for to in (from + 1)..node_count {
            if static_mask[idx] {
                static_edges.push((from, to));
                if reexport_mask[idx] {
                    reexport_static_edges.push((from, to));
                }
            }
            idx += 1;
        }
    }
    let mut dynamic_idx = 0;
    for from in 0..node_count {
        for to in 0..node_count {
            if from == to {
                continue;
            }
            for _ in 0..dynamic_mask[dynamic_idx] {
                dynamic_edges.push((from, to));
            }
            dynamic_idx += 1;
        }
    }

    let mut external_dynamic_edges = Vec::new();
    let mut ext_idx = 0;
    for from in 0..node_count {
        for ext in 0..external_module_count {
            if external_dynamic_mask[ext_idx] {
                external_dynamic_edges.push((from, ext));
            }
            ext_idx += 1;
        }
    }

    GraphCase {
        seed,
        node_count,
        entry_nodes,
        cjs_nodes,
        static_edges,
        dynamic_edges,
        reexport_static_edges,
        external_module_count,
        external_dynamic_edges,
        preserve_entry_signatures,
        strict_execution_order,
        treeshake,
        minify_internal_exports,
    }
}

struct SeedRng {
    state: u64,
}

impl SeedRng {
    fn new(seed: u64) -> Self {
        let state = if seed == 0 {
            0x9E37_79B9_7F4A_7C15
        } else {
            seed
        };
        Self { state }
    }

    fn next_u64(&mut self) -> u64 {
        self.state ^= self.state >> 12;
        self.state ^= self.state << 25;
        self.state ^= self.state >> 27;
        self.state = self.state.wrapping_mul(0x2545_F491_4F6C_DD1D);
        self.state
    }

    fn next_bool(&mut self, numerator: u64, denominator: u64) -> bool {
        self.next_u64() % denominator < numerator
    }
}

fn case_from_seed(seed: u64) -> GraphCase {
    let mut rng = SeedRng::new(seed);
    let node_count = 3 + (rng.next_u64() as usize % (MAX_NODES - 2));
    let preserve_entry_signatures_index = (rng.next_u64() % 4) as u8;
    let strict_execution_order = rng.next_bool(1, 2);
    let treeshake = rng.next_bool(1, 2);
    let minify_internal_exports = rng.next_bool(1, 2);
    let external_module_count = 1 + rng.next_u64() as usize % MAX_EXTERNAL_MODULES;

    let entry_mask = (0..node_count)
        .map(|_| rng.next_bool(1, 2))
        .collect::<Vec<_>>();
    let cjs_mask = (0..node_count)
        .map(|_| rng.next_bool(1, 3))
        .collect::<Vec<_>>();

    let static_edge_slots = node_count * (node_count - 1) / 2;
    let static_mask = (0..static_edge_slots)
        .map(|_| rng.next_bool(2, 5))
        .collect::<Vec<_>>();
    let reexport_mask = (0..static_edge_slots)
        .map(|_| rng.next_bool(1, 2))
        .collect::<Vec<_>>();

    let dynamic_edge_slots = node_count * (node_count - 1);
    let dynamic_mask = (0..dynamic_edge_slots)
        .map(|_| (rng.next_u64() % 3) as u8)
        .collect::<Vec<_>>();

    let external_dynamic_slots = node_count * external_module_count;
    let external_dynamic_mask = (0..external_dynamic_slots)
        .map(|_| rng.next_bool(1, 3))
        .collect::<Vec<_>>();

    build_case_from_masks(
        seed,
        node_count,
        preserve_entry_signatures_index,
        strict_execution_order,
        treeshake,
        minify_internal_exports,
        &entry_mask,
        &cjs_mask,
        &static_mask,
        &dynamic_mask,
        &reexport_mask,
        external_module_count,
        &external_dynamic_mask,
    )
}

fn validate_output_js_syntax(output: &rolldown::BundleOutput) -> Result<(), String> {
    for asset in &output.assets {
        let Output::Chunk(chunk) = asset else {
            continue;
        };
        let allocator = OxcAllocator::default();
        let source_type = SourceType::mjs();
        let ret = Parser::new(&allocator, &chunk.code, source_type)
            .with_options(ParseOptions {
                allow_return_outside_function: true,
                ..ParseOptions::default()
            })
            .parse();
        if ret.panicked || ret.diagnostics.errors().next().is_some() {
            let errors_str = ret
                .diagnostics
                .errors()
                .map(|e| e.clone().with_source_code(chunk.code.clone()).to_string())
                .collect::<Vec<_>>()
                .join("\n");
            return Err(format!(
                "Syntax error in output chunk '{}':\n{}",
                chunk.filename, errors_str
            ));
        }
    }
    Ok(())
}

fn compute_expected_exports(case: &GraphCase, node_index: usize) -> Vec<String> {
    let is_cjs = is_cjs_node(case, node_index);
    if is_cjs {
        // CJS modules get wrapped; their exports are opaque to the bundler
        return Vec::new();
    }

    let reexport_set: HashSet<(usize, usize)> =
        case.reexport_static_edges.iter().copied().collect();
    let mut exports = Vec::new();
    exports.push(format!("node_{node_index}"));

    for &(from, to) in &case.static_edges {
        if from != node_index {
            continue;
        }
        if reexport_set.contains(&(from, to)) && !is_cjs_node(case, to) {
            exports.push(format!("reexport_{from}_{to}"));
        } else {
            exports.push(format!("use_{from}_{to}"));
        }
    }

    exports.sort();
    exports
}

fn validate_entry_exports(case: &GraphCase, output: &rolldown::BundleOutput) -> Result<(), String> {
    if !matches!(
        case.preserve_entry_signatures,
        PreserveEntrySignatures::Strict
    ) {
        return Ok(());
    }

    let entry_set: HashSet<usize> = case.entry_nodes.iter().copied().collect();
    for asset in &output.assets {
        let Output::Chunk(chunk) = asset else {
            continue;
        };
        if !chunk.is_entry {
            continue;
        }
        let facade_id = match &chunk.facade_module_id {
            Some(id) => id.to_string(),
            None => continue,
        };

        // Find which input node this entry chunk corresponds to
        let node_index = case.entry_nodes.iter().copied().find(|&idx| {
            entry_set.contains(&idx) && facade_id.ends_with(&module_filename(case, idx))
        });
        let Some(node_index) = node_index else {
            continue;
        };

        let expected = compute_expected_exports(case, node_index);
        if expected.is_empty() {
            continue;
        }

        let actual: HashSet<String> = chunk.exports.iter().map(|e| e.to_string()).collect();
        let mut missing = Vec::new();
        for exp in &expected {
            if !actual.contains(exp) {
                missing.push(exp.clone());
            }
        }
        if !missing.is_empty() {
            return Err(format!(
                "Entry chunk '{}' (node {}) is missing exports: {:?}\nExpected: {:?}\nActual: {:?}",
                chunk.filename, node_index, missing, expected, actual
            ));
        }
    }

    Ok(())
}

async fn run_case(case: GraphCase) -> Result<(), String> {
    if let Some(input_cycle) = graph_cycle_checker::find_cycle(case.node_count, &case.static_edges)
    {
        let path = input_cycle
            .iter()
            .map(|index| format!("`node{index}`"))
            .collect::<Vec<_>>()
            .join(" -> ");
        return Err(format!(
            "generated static input graph was unexpectedly cyclic at path: {path}"
        ));
    }

    let fixture_dir = create_fixture_dir(case.seed).map_err(|err| err.to_string())?;
    materialize_graph_modules(&fixture_dir, &case).map_err(|err| err.to_string())?;
    let options = bundler_options_for_case(&case, fixture_dir.clone());

    let mut bundler = Bundler::new(options.clone()).map_err(|err| err.to_string())?;
    let output = match bundler.generate().await {
        Ok(output) => output,
        Err(err) => {
            return Err(format_failure_message(
                &case,
                &fixture_dir,
                &options,
                &[],
                &[],
                &[],
                None,
                Some(err.to_string()),
            ));
        }
    };

    let (output_files, output_static_edges, output_dynamic_edges, chunk_has_cjs) =
        build_output_dependency_graph(&output);
    let output_static_cycle =
        graph_cycle_checker::find_cycle(output_files.len(), &output_static_edges);

    if let Some(cycle) = output_static_cycle.as_ref() {
        // An *immediate* (two-chunk) cycle that involves a CommonJS module is an
        // inherent CJS/ESM interop limitation rather than a Rolldown defect: a CJS
        // and an ESM chunk that import each other directly form the immediate cycle
        // that Node.js rejects at runtime with `ERR_REQUIRE_CYCLE_MODULE`. Larger
        // cycles, and cycles between pure-ESM chunks, are still genuine Rolldown
        // output-graph defects and must be reported.
        let is_cjs_immediate_cycle =
            is_immediate_cycle(cycle) && cycle_involves_cjs(cycle, &chunk_has_cjs);
        if !is_cjs_immediate_cycle {
            return Err(format_failure_message(
                &case,
                &fixture_dir,
                &options,
                &output_files,
                &output_static_edges,
                &output_dynamic_edges,
                Some(cycle),
                None,
            ));
        }
    }

    validate_output_js_syntax(&output)?;
    if !case.treeshake {
        validate_entry_exports(&case, &output)?;
    }

    std::fs::remove_dir_all(&fixture_dir).map_err(|err| err.to_string())?;
    Ok(())
}

async fn run_deterministic_check(case: GraphCase) -> Result<(), String> {
    if graph_cycle_checker::find_cycle(case.node_count, &case.static_edges).is_some() {
        // Skip cyclic inputs (shouldn't happen but guard anyway)
        return Ok(());
    }

    let fixture_dir = create_fixture_dir(case.seed).map_err(|err| err.to_string())?;
    materialize_graph_modules(&fixture_dir, &case).map_err(|err| err.to_string())?;
    let options = bundler_options_for_case(&case, fixture_dir.clone());

    let output_a = {
        let mut bundler = Bundler::new(options.clone()).map_err(|err| err.to_string())?;
        bundler.generate().await.map_err(|err| err.to_string())?
    };

    let output_b = {
        let mut bundler = Bundler::new(options.clone()).map_err(|err| err.to_string())?;
        bundler.generate().await.map_err(|err| err.to_string())?
    };

    let chunks_a = collect_chunk_info(&output_a);
    let chunks_b = collect_chunk_info(&output_b);

    if chunks_a.len() != chunks_b.len() {
        std::fs::remove_dir_all(&fixture_dir).ok();
        return Err(format!(
            "Nondeterministic output: first run produced {} chunks, second run produced {} chunks (seed {})",
            chunks_a.len(),
            chunks_b.len(),
            case.seed,
        ));
    }

    for (a, b) in chunks_a.iter().zip(chunks_b.iter()) {
        if a.filename != b.filename {
            std::fs::remove_dir_all(&fixture_dir).ok();
            return Err(format!(
                "Nondeterministic output: chunk filenames differ: '{}' vs '{}' (seed {})",
                a.filename, b.filename, case.seed,
            ));
        }
        if a.code != b.code {
            std::fs::remove_dir_all(&fixture_dir).ok();
            return Err(format!(
                "Nondeterministic output: code differs for chunk '{}' (seed {})",
                a.filename, case.seed,
            ));
        }
        if a.imports != b.imports {
            std::fs::remove_dir_all(&fixture_dir).ok();
            return Err(format!(
                "Nondeterministic output: imports differ for chunk '{}' (seed {})",
                a.filename, case.seed,
            ));
        }
        if a.exports != b.exports {
            std::fs::remove_dir_all(&fixture_dir).ok();
            return Err(format!(
                "Nondeterministic output: exports differ for chunk '{}' (seed {})",
                a.filename, case.seed,
            ));
        }
        if a.dynamic_imports != b.dynamic_imports {
            std::fs::remove_dir_all(&fixture_dir).ok();
            return Err(format!(
                "Nondeterministic output: dynamic_imports differ for chunk '{}' (seed {})",
                a.filename, case.seed,
            ));
        }
    }

    std::fs::remove_dir_all(&fixture_dir).map_err(|err| err.to_string())?;
    Ok(())
}

struct ChunkInfo {
    filename: String,
    code: String,
    imports: Vec<String>,
    exports: Vec<String>,
    dynamic_imports: Vec<String>,
}

fn collect_chunk_info(output: &rolldown::BundleOutput) -> Vec<ChunkInfo> {
    let mut chunks: Vec<ChunkInfo> = output
        .assets
        .iter()
        .filter_map(|asset| match asset {
            Output::Chunk(chunk) => Some(ChunkInfo {
                filename: chunk.filename.to_string(),
                code: chunk.code.clone(),
                imports: chunk.imports.iter().map(|s| s.to_string()).collect(),
                exports: chunk.exports.iter().map(|s| s.to_string()).collect(),
                dynamic_imports: chunk
                    .dynamic_imports
                    .iter()
                    .map(|s| s.to_string())
                    .collect(),
            }),
            Output::Asset(_) => None,
        })
        .collect();
    chunks.sort_by(|a, b| a.filename.cmp(&b.filename));
    chunks
}

fn input_items_for_case(case: &GraphCase) -> Vec<InputItem> {
    case.entry_nodes
        .iter()
        .copied()
        .map(|index| InputItem {
            name: Some(format!("entry-{index}")),
            import: format!("./{}", module_filename(case, index)),
        })
        .collect::<Vec<_>>()
}

fn bundler_options_for_case(case: &GraphCase, cwd: PathBuf) -> BundlerOptions {
    let external = if case.external_module_count > 0 {
        Some(IsExternal::from(
            (0..case.external_module_count)
                .map(external_module_name)
                .collect::<Vec<_>>(),
        ))
    } else {
        None
    };
    BundlerOptions {
        input: Some(input_items_for_case(case)),
        cwd: Some(cwd),
        format: Some(OutputFormat::Esm),
        treeshake: TreeshakeOptions::Boolean(case.treeshake),
        preserve_entry_signatures: Some(case.preserve_entry_signatures),
        strict_execution_order: Some(case.strict_execution_order),
        minify_internal_exports: Some(case.minify_internal_exports),
        external,
        ..Default::default()
    }
}

fn build_output_dependency_graph(
    output: &rolldown::BundleOutput,
) -> (
    Vec<String>,
    Vec<(usize, usize)>,
    Vec<(usize, usize)>,
    Vec<bool>,
) {
    let chunks = output
        .assets
        .iter()
        .filter_map(|asset| match asset {
            Output::Chunk(chunk) => Some((
                chunk.filename.to_string(),
                chunk
                    .imports
                    .iter()
                    .map(ToString::to_string)
                    .collect::<Vec<_>>(),
                chunk
                    .dynamic_imports
                    .iter()
                    .map(ToString::to_string)
                    .collect::<Vec<_>>(),
                chunk_contains_cjs_module(chunk),
            )),
            Output::Asset(_) => None,
        })
        .collect::<Vec<_>>();

    let output_files = chunks
        .iter()
        .map(|(filename, _, _, _)| filename.clone())
        .collect::<Vec<_>>();
    let chunk_has_cjs = chunks
        .iter()
        .map(|(_, _, _, has_cjs)| *has_cjs)
        .collect::<Vec<_>>();
    let output_index = output_files
        .iter()
        .enumerate()
        .map(|(index, filename)| (filename.clone(), index))
        .collect::<HashMap<_, _>>();

    let mut static_edges = Vec::new();
    let mut dynamic_edges = Vec::new();
    for (from_filename, imports, dynamic_imports, _) in chunks {
        let Some(from_index) = output_index.get(&from_filename).copied() else {
            continue;
        };

        for import_specifier in imports {
            let resolved = resolve_specifier(&from_filename, &import_specifier);
            if let Some(to_index) = output_index.get(&resolved).copied() {
                static_edges.push((from_index, to_index));
            }
        }

        for import_specifier in dynamic_imports {
            let resolved = resolve_specifier(&from_filename, &import_specifier);
            if let Some(to_index) = output_index.get(&resolved).copied() {
                dynamic_edges.push((from_index, to_index));
            }
        }
    }

    (output_files, static_edges, dynamic_edges, chunk_has_cjs)
}

/// Whether an output chunk contains at least one CommonJS source module.
///
/// CJS source modules are materialized with a `.cjs` extension (see
/// [`module_filename`]), so a chunk owning such a module forces Rolldown to emit
/// the CommonJS interop wrappers (`__commonJSMin`, …). When a CJS chunk and an
/// ESM chunk import each other directly, that immediate cycle reproduces the
/// CJS↔ESM boundary cycle Node.js rejects at runtime with
/// `ERR_REQUIRE_CYCLE_MODULE` (see [`is_immediate_cycle`] and
/// [`cycle_involves_cjs`]).
fn chunk_contains_cjs_module(chunk: &rolldown_common::OutputChunk) -> bool {
    chunk
        .module_ids
        .iter()
        .any(|module_id| module_id.as_str().ends_with(".cjs"))
}

/// Whether any chunk along the given output cycle owns a CommonJS module.
fn cycle_involves_cjs(cycle: &[usize], chunk_has_cjs: &[bool]) -> bool {
    cycle
        .iter()
        .any(|&index| chunk_has_cjs.get(index).copied().unwrap_or(false))
}

/// Whether the cycle is *immediate*: exactly two chunks that import each other
/// directly. [`graph_cycle_checker::find_cycle`] returns the path with the entry
/// chunk repeated at both ends (e.g. `[a, b, a]`), so an immediate cycle visits
/// exactly two distinct chunks. Only immediate cycles map to the CJS↔ESM
/// `ERR_REQUIRE_CYCLE_MODULE` boundary; larger cycles stay reportable.
fn is_immediate_cycle(cycle: &[usize]) -> bool {
    cycle.iter().copied().collect::<HashSet<_>>().len() == 2
}

fn resolve_specifier(importer: &str, specifier: &str) -> String {
    if !specifier.starts_with('.') {
        return specifier.to_string();
    }

    let importer_parent = Path::new(importer)
        .parent()
        .unwrap_or_else(|| Path::new(""));
    normalize_path(importer_parent.join(specifier))
}

fn normalize_path(path: PathBuf) -> String {
    let mut normalized = Vec::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                normalized.pop();
            }
            Component::Normal(segment) => normalized.push(segment.to_string_lossy().to_string()),
            Component::Prefix(_) | Component::RootDir => {}
        }
    }
    normalized.join("/")
}

fn is_cjs_node(case: &GraphCase, index: usize) -> bool {
    case.cjs_nodes.binary_search(&index).is_ok()
}

fn module_filename(case: &GraphCase, index: usize) -> String {
    if is_cjs_node(case, index) {
        format!("node{index}.cjs")
    } else {
        format!("node{index}.js")
    }
}

fn render_graph_modules(case: &GraphCase) -> Vec<(String, String)> {
    let mut static_outgoing = vec![Vec::<usize>::new(); case.node_count];
    for &(from, to) in &case.static_edges {
        static_outgoing[from].push(to);
    }
    let mut dynamic_outgoing = vec![Vec::<usize>::new(); case.node_count];
    for &(from, to) in &case.dynamic_edges {
        dynamic_outgoing[from].push(to);
    }
    let mut external_dynamic_outgoing = vec![Vec::<usize>::new(); case.node_count];
    for &(from, ext_index) in &case.external_dynamic_edges {
        external_dynamic_outgoing[from].push(ext_index);
    }
    let reexport_static_edges = case
        .reexport_static_edges
        .iter()
        .copied()
        .collect::<HashSet<_>>();

    let mut modules = Vec::new();
    for (from, destinations) in static_outgoing.iter().enumerate() {
        let current_file_is_cjs = is_cjs_node(case, from);
        let mut source = String::new();
        source.push_str(&format!(
            "globalThis.__acyclic_output_fuzz_{from} = {from};\n"
        ));
        for destination in destinations {
            let destination_path = module_filename(case, *destination);
            if current_file_is_cjs {
                source.push_str(&format!(
                    "const imported_{from}_{destination} = require(\"./{destination_path}\");\n"
                ));
                source.push_str(&format!(
                    "exports.use_{from}_{destination} = imported_{from}_{destination};\n"
                ));
            } else if reexport_static_edges.contains(&(from, *destination))
                && !is_cjs_node(case, *destination)
            {
                source.push_str(&format!(
                    "export {{ node_{destination} as reexport_{from}_{destination} }} from \"./{destination_path}\";\n"
                ));
            } else {
                source.push_str(&format!(
                    "import * as imported_{from}_{destination} from \"./{destination_path}\";\n"
                ));
                source.push_str(&format!(
                    "var use_{from}_{destination} = imported_{from}_{destination};\n"
                ));
                source.push_str(&format!("use_{from}_{destination}.ref = {from};\n"));
                source.push_str(&format!("export {{ use_{from}_{destination} }};\n"));
            }
        }
        for destination in &dynamic_outgoing[from] {
            let destination_path = module_filename(case, *destination);
            if current_file_is_cjs {
                source.push_str(&format!("void require(\"./{destination_path}\");\n"));
            } else {
                source.push_str(&format!("void import(\"./{destination_path}\");\n"));
            }
        }
        for ext_index in &external_dynamic_outgoing[from] {
            let ext_name = external_module_name(*ext_index);
            if current_file_is_cjs {
                source.push_str(&format!("void require(\"{ext_name}\");\n"));
            } else {
                source.push_str(&format!("void import(\"{ext_name}\");\n"));
            }
        }
        if current_file_is_cjs {
            source.push_str(&format!("exports.node_{from} = {from};\n"));
        } else {
            source.push_str(&format!("var node_{from} = {{}};\n"));
            source.push_str(&format!("node_{from}.value = {from};\n"));
            source.push_str(&format!("export {{ node_{from} }};\n"));
        }
        modules.push((module_filename(case, from), source));
    }

    modules
}

fn materialize_graph_modules(dir: &Path, case: &GraphCase) -> std::io::Result<()> {
    for (filename, source) in render_graph_modules(case) {
        std::fs::write(dir.join(filename), source)?;
    }
    Ok(())
}

fn create_fixture_dir(seed: u64) -> std::io::Result<PathBuf> {
    let timestamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let path = std::env::temp_dir()
        .join("rolldown-acyclic-output-fuzz")
        .join(format!("seed-{seed}-ts-{timestamp}"));
    std::fs::create_dir_all(&path)?;
    Ok(path)
}

fn preserve_entry_signatures_to_config_value(value: PreserveEntrySignatures) -> &'static str {
    match value {
        PreserveEntrySignatures::AllowExtension => "\"allow-extension\"",
        PreserveEntrySignatures::Strict => "\"strict\"",
        PreserveEntrySignatures::ExportsOnly => "\"exports-only\"",
        PreserveEntrySignatures::False => "false",
    }
}

fn treeshake_to_config_value(value: &TreeshakeOptions) -> &'static str {
    match value {
        TreeshakeOptions::Boolean(false) => "false",
        TreeshakeOptions::Boolean(true) => "true",
        TreeshakeOptions::Option(_) => "true",
    }
}

fn preserve_entry_signatures_to_index(value: PreserveEntrySignatures) -> u8 {
    match value {
        PreserveEntrySignatures::AllowExtension => 0,
        PreserveEntrySignatures::Strict => 1,
        PreserveEntrySignatures::ExportsOnly => 2,
        PreserveEntrySignatures::False => 3,
    }
}

fn normalize_entry_nodes(mut entries: Vec<usize>, node_count: usize, seed: u64) -> Vec<usize> {
    entries.retain(|index| *index < node_count);
    entries.sort_unstable();
    entries.dedup();
    if entries.is_empty() {
        entries.push((seed as usize) % node_count);
    } else if entries.len() == node_count && node_count > 1 {
        entries.pop();
    }
    entries
}

fn normalize_node_set(mut nodes: Vec<usize>, node_count: usize) -> Vec<usize> {
    nodes.retain(|index| *index < node_count);
    nodes.sort_unstable();
    nodes.dedup();
    nodes
}

fn normalize_edges(mut edges: Vec<(usize, usize)>, node_count: usize) -> Vec<(usize, usize)> {
    edges.retain(|(from, to)| *from < node_count && *to < node_count && from != to);
    edges.sort_unstable();
    edges.dedup();
    edges
}

fn normalize_edges_allow_dupes(
    mut edges: Vec<(usize, usize)>,
    node_count: usize,
) -> Vec<(usize, usize)> {
    edges.retain(|(from, to)| *from < node_count && *to < node_count && from != to);
    edges.sort_unstable();
    edges
}

fn encode_node_list(nodes: &[usize]) -> String {
    if nodes.is_empty() {
        "none".to_string()
    } else {
        nodes
            .iter()
            .map(|index| index.to_string())
            .collect::<Vec<_>>()
            .join(",")
    }
}

fn encode_edge_list(edges: &[(usize, usize)]) -> String {
    if edges.is_empty() {
        "none".to_string()
    } else {
        edges
            .iter()
            .map(|(from, to)| format!("{from}-{to}"))
            .collect::<Vec<_>>()
            .join(",")
    }
}

fn decode_node_list(value: &str) -> Result<Vec<usize>, String> {
    if value.is_empty() || value == "none" {
        return Ok(Vec::new());
    }
    value
        .split(',')
        .map(|part| {
            part.parse::<usize>()
                .map_err(|_| format!("invalid node index `{part}`"))
        })
        .collect::<Result<Vec<_>, _>>()
}

fn decode_edge_list(value: &str) -> Result<Vec<(usize, usize)>, String> {
    if value.is_empty() || value == "none" {
        return Ok(Vec::new());
    }

    value
        .split(',')
        .map(|part| {
            let (from, to) = part
                .split_once('-')
                .ok_or_else(|| format!("invalid edge `{part}`"))?;
            let from = from
                .parse::<usize>()
                .map_err(|_| format!("invalid edge source `{from}`"))?;
            let to = to
                .parse::<usize>()
                .map_err(|_| format!("invalid edge destination `{to}`"))?;
            Ok((from, to))
        })
        .collect::<Result<Vec<_>, _>>()
}

fn encode_external_spec(count: usize, edges: &[(usize, usize)]) -> String {
    if count == 0 {
        "none".to_string()
    } else {
        format!("{count}:{}", encode_edge_list(edges))
    }
}

fn decode_external_spec(value: &str) -> Result<(usize, Vec<(usize, usize)>), String> {
    if value.is_empty() || value == "none" {
        return Ok((0, Vec::new()));
    }
    let (count_str, edges_str) = value
        .split_once(':')
        .ok_or_else(|| format!("invalid external spec `{value}`"))?;
    let count = count_str
        .parse::<usize>()
        .map_err(|_| format!("invalid external module count `{count_str}`"))?;
    let edges = decode_edge_list(edges_str)?;
    Ok((count, edges))
}

fn encode_case_spec(case: &GraphCase) -> String {
    format!(
        "n={n};e={e};c={c};s={s};d={d};r={r};x={x};p={p};o={o};t={t};m={m}",
        n = case.node_count,
        e = encode_node_list(&case.entry_nodes),
        c = encode_node_list(&case.cjs_nodes),
        s = encode_edge_list(&case.static_edges),
        d = encode_edge_list(&case.dynamic_edges),
        r = encode_edge_list(&case.reexport_static_edges),
        x = encode_external_spec(case.external_module_count, &case.external_dynamic_edges),
        p = preserve_entry_signatures_to_index(case.preserve_entry_signatures),
        o = usize::from(case.strict_execution_order),
        t = usize::from(case.treeshake),
        m = usize::from(case.minify_internal_exports),
    )
}

fn parse_case_spec(seed: u64, case_spec: &str) -> Result<GraphCase, String> {
    let mut node_count = None;
    let mut entries = None;
    let mut cjs_nodes = None;
    let mut static_edges = None;
    let mut dynamic_edges = None;
    let mut reexport_edges = None;
    let mut external_spec = None;
    let mut preserve_index = None;
    let mut strict_execution_order = None;
    let mut treeshake = None;
    let mut minify_internal_exports = None;

    for part in case_spec.split(';') {
        if part.is_empty() {
            continue;
        }
        let (key, value) = part
            .split_once('=')
            .ok_or_else(|| format!("invalid case segment `{part}`"))?;
        match key {
            "n" => {
                node_count = Some(
                    value
                        .parse::<usize>()
                        .map_err(|_| format!("invalid node count `{value}`"))?,
                );
            }
            "e" => {
                entries = Some(decode_node_list(value)?);
            }
            "c" => {
                cjs_nodes = Some(decode_node_list(value)?);
            }
            "s" => {
                static_edges = Some(decode_edge_list(value)?);
            }
            "d" => {
                dynamic_edges = Some(decode_edge_list(value)?);
            }
            "r" => {
                reexport_edges = Some(decode_edge_list(value)?);
            }
            "x" => {
                external_spec = Some(decode_external_spec(value)?);
            }
            "p" => {
                preserve_index = Some(
                    value
                        .parse::<u8>()
                        .map_err(|_| format!("invalid preserveEntrySignatures index `{value}`"))?,
                );
            }
            "o" => {
                strict_execution_order = Some(match value {
                    "1" | "true" => true,
                    "0" | "false" => false,
                    _ => {
                        return Err(format!("invalid strictExecutionOrder `{value}`"));
                    }
                });
            }
            "t" => {
                treeshake = Some(match value {
                    "1" | "true" => true,
                    "0" | "false" => false,
                    _ => {
                        return Err(format!("invalid treeshake `{value}`"));
                    }
                });
            }
            "m" => {
                minify_internal_exports = Some(match value {
                    "1" | "true" => true,
                    "0" | "false" => false,
                    _ => {
                        return Err(format!("invalid minifyInternalExports `{value}`"));
                    }
                });
            }
            _ => return Err(format!("unknown case key `{key}`")),
        }
    }

    let node_count = node_count.ok_or_else(|| "missing case field `n`".to_string())?;
    if !(1..=MAX_NODES).contains(&node_count) {
        return Err(format!(
            "node count `{node_count}` is outside 1..={MAX_NODES}"
        ));
    }
    let entry_nodes = normalize_entry_nodes(
        entries.ok_or_else(|| "missing case field `e`".to_string())?,
        node_count,
        seed,
    );
    let cjs_nodes = normalize_node_set(cjs_nodes.unwrap_or_default(), node_count);
    let static_edges = normalize_edges(
        static_edges.ok_or_else(|| "missing case field `s`".to_string())?,
        node_count,
    );
    let dynamic_edges = normalize_edges_allow_dupes(
        dynamic_edges.ok_or_else(|| "missing case field `d`".to_string())?,
        node_count,
    );
    let (external_module_count, external_dynamic_edges) = external_spec.unwrap_or((0, Vec::new()));
    let external_dynamic_edges = {
        let mut edges = external_dynamic_edges;
        edges.retain(|(from, ext)| *from < node_count && *ext < external_module_count);
        edges.sort_unstable();
        edges
    };
    let static_edge_set = static_edges.iter().copied().collect::<HashSet<_>>();
    let reexport_static_edges = normalize_edges(
        reexport_edges.ok_or_else(|| "missing case field `r`".to_string())?,
        node_count,
    )
    .into_iter()
    .filter(|edge| static_edge_set.contains(edge))
    .collect::<Vec<_>>();
    let preserve_entry_signatures = preserve_entry_signatures_from_index(
        preserve_index.ok_or_else(|| "missing case field `p`".to_string())?,
    );
    let strict_execution_order =
        strict_execution_order.ok_or_else(|| "missing case field `o`".to_string())?;
    let treeshake = treeshake.unwrap_or(false);
    let minify_internal_exports = minify_internal_exports.unwrap_or(true);

    Ok(GraphCase {
        seed,
        node_count,
        entry_nodes,
        cjs_nodes,
        static_edges,
        dynamic_edges,
        reexport_static_edges,
        external_module_count,
        external_dynamic_edges,
        preserve_entry_signatures,
        strict_execution_order,
        treeshake,
        minify_internal_exports,
    })
}

fn write_rolldown_config_js(
    dir: &Path,
    case: &GraphCase,
    options: &BundlerOptions,
) -> std::io::Result<()> {
    let config = render_rolldown_config_js(case, options);
    std::fs::write(dir.join("rolldown.config.js"), config)
}

fn render_rolldown_config_js(case: &GraphCase, options: &BundlerOptions) -> String {
    let input_entries = input_items_for_case(case)
        .into_iter()
        .map(|item| {
            let name = item.name.unwrap_or_else(|| "entry".to_string());
            format!("    \"{name}\": \"{}\"", item.import)
        })
        .collect::<Vec<_>>()
        .join(",\n");

    let preserve_entry_signatures = preserve_entry_signatures_to_config_value(
        options
            .preserve_entry_signatures
            .unwrap_or(PreserveEntrySignatures::ExportsOnly),
    );
    let treeshake = treeshake_to_config_value(&options.treeshake);
    let strict_execution_order = options.strict_execution_order.unwrap_or(false);
    let minify_internal_exports = options.minify_internal_exports.unwrap_or(true);

    let external_line = if case.external_module_count > 0 {
        let names = (0..case.external_module_count)
            .map(|i| format!("\"{}\"", external_module_name(i)))
            .collect::<Vec<_>>()
            .join(", ");
        format!("  external: [{names}],\n")
    } else {
        String::new()
    };

    let config = format!(
        concat!(
            "// Generated by `cargo run -p acyclic_output_fuzz --bin generate_fixture -- --seed {seed}`\n",
            "export default {{\n",
            "  input: {{\n",
            "{inputs}\n",
            "  }},\n",
            "{external}",
            "  treeshake: {treeshake},\n",
            "  preserveEntrySignatures: {preserve_entry_signatures},\n",
            "  output: {{\n",
            "    strictExecutionOrder: {strict_execution_order},\n",
            "    minifyInternalExports: {minify_internal_exports},\n",
            "  }},\n",
            "}};\n"
        ),
        seed = case.seed,
        inputs = input_entries,
        external = external_line,
        treeshake = treeshake,
        preserve_entry_signatures = preserve_entry_signatures,
        strict_execution_order = strict_execution_order,
        minify_internal_exports = minify_internal_exports,
    );
    config
}

pub fn generate_fixture_from_seed(
    seed: u64,
    output_dir: Option<PathBuf>,
) -> std::io::Result<PathBuf> {
    let case = case_from_seed(seed);
    let dir = if let Some(output_dir) = output_dir {
        std::fs::create_dir_all(&output_dir)?;
        output_dir
    } else {
        create_fixture_dir(seed)?
    };

    materialize_graph_modules(&dir, &case)?;
    let options = bundler_options_for_case(&case, dir.clone());
    write_rolldown_config_js(&dir, &case, &options)?;

    Ok(dir)
}

pub fn generate_fixture_from_case_spec(
    seed: u64,
    case_spec: &str,
    output_dir: Option<PathBuf>,
) -> Result<PathBuf, String> {
    let case = parse_case_spec(seed, case_spec)?;
    let dir = if let Some(output_dir) = output_dir {
        std::fs::create_dir_all(&output_dir).map_err(|err| err.to_string())?;
        output_dir
    } else {
        create_fixture_dir(seed).map_err(|err| err.to_string())?
    };

    materialize_graph_modules(&dir, &case).map_err(|err| err.to_string())?;
    let options = bundler_options_for_case(&case, dir.clone());
    write_rolldown_config_js(&dir, &case, &options).map_err(|err| err.to_string())?;

    Ok(dir)
}

fn build_repl_url(case: &GraphCase, options: &BundlerOptions) -> String {
    let entry_files = case
        .entry_nodes
        .iter()
        .map(|index| module_filename(case, *index))
        .collect::<HashSet<_>>();

    let mut files = serde_json::Map::new();
    for (filename, source) in render_graph_modules(case) {
        let file_json = if entry_files.contains(&filename) {
            serde_json::json!({
                "n": filename,
                "c": source,
                "e": true,
            })
        } else {
            serde_json::json!({
                "n": filename,
                "c": source,
            })
        };
        files.insert(filename, file_json);
    }

    let config_filename = "rolldown.config.js".to_string();
    files.insert(
        config_filename.clone(),
        serde_json::json!({
            "n": config_filename,
            "c": render_rolldown_config_js(case, options),
        }),
    );

    let state = serde_json::json!({
        "f": files,
        "v": "latest",
    });

    let serialized = match serde_json::to_string(&state) {
        Ok(serialized) => serialized,
        Err(_) => return "https://repl.rolldown.rs/".to_string(),
    };
    let encoded = encode_repl_hash(&serialized);
    format!("https://repl.rolldown.rs/#{encoded}")
}

fn encode_repl_hash(serialized: &str) -> String {
    let mut encoder = ZlibEncoder::new(Vec::new(), Compression::best());
    if encoder.write_all(serialized.as_bytes()).is_ok() {
        if let Ok(compressed) = encoder.finish() {
            return BASE64_STANDARD.encode(compressed);
        }
    }

    BASE64_STANDARD.encode(serialized.as_bytes())
}

fn format_failure_message(
    case: &GraphCase,
    _fixture_dir: &Path,
    options: &BundlerOptions,
    output_files: &[String],
    output_static_edges: &[(usize, usize)],
    output_dynamic_edges: &[(usize, usize)],
    output_static_cycle: Option<&[usize]>,
    _build_error: Option<String>,
) -> String {
    let output_static_edge_lines = output_static_edges
        .iter()
        .filter_map(|(from, to)| {
            Some(format!(
                "- `{}` -> `{}`",
                output_files.get(*from)?,
                output_files.get(*to)?
            ))
        })
        .collect::<Vec<_>>();
    let output_dynamic_edge_lines = output_dynamic_edges
        .iter()
        .filter_map(|(from, to)| {
            Some(format!(
                "- `{}` -> `{}`",
                output_files.get(*from)?,
                output_files.get(*to)?
            ))
        })
        .collect::<Vec<_>>();
    let input_static_edge_lines = case
        .static_edges
        .iter()
        .map(|(from, to)| format!("- `node{from}` -> `node{to}`"))
        .collect::<Vec<_>>();
    let input_dynamic_edge_lines = case
        .dynamic_edges
        .iter()
        .map(|(from, to)| format!("- `node{from}` -> `node{to}`"))
        .collect::<Vec<_>>();
    let input_reexport_edge_lines = case
        .reexport_static_edges
        .iter()
        .map(|(from, to)| format!("- `node{from}` -> `node{to}`"))
        .collect::<Vec<_>>();
    let input_external_dynamic_edge_lines = case
        .external_dynamic_edges
        .iter()
        .map(|(from, ext)| format!("- `node{from}` -> `{}`", external_module_name(*ext)))
        .collect::<Vec<_>>();
    let output_file_lines = output_files
        .iter()
        .map(|file| format!("- `{file}`"))
        .collect::<Vec<_>>();

    let case_spec = encode_case_spec(case);
    let repl_url = build_repl_url(case, options);
    let fixture_command = format!(
        "cargo run -p acyclic_output_fuzz --bin generate_fixture -- --seed {} --case '{}' --out ./fixtures/seed-{}",
        case.seed, case_spec, case.seed
    );
    let input_static_edges = if input_static_edge_lines.is_empty() {
        "- (none)".to_string()
    } else {
        input_static_edge_lines.join("\n")
    };
    let input_reexport_edges = if input_reexport_edge_lines.is_empty() {
        "- (none)".to_string()
    } else {
        input_reexport_edge_lines.join("\n")
    };
    let input_dynamic_edges = if input_dynamic_edge_lines.is_empty() {
        "- (none)".to_string()
    } else {
        input_dynamic_edge_lines.join("\n")
    };
    let input_external_dynamic_edges = if input_external_dynamic_edge_lines.is_empty() {
        "- (none)".to_string()
    } else {
        input_external_dynamic_edge_lines.join("\n")
    };
    let output_files_markdown = if output_file_lines.is_empty() {
        "- (none)".to_string()
    } else {
        output_file_lines.join("\n")
    };
    let output_static_edges = if output_static_edge_lines.is_empty() {
        "- (none)".to_string()
    } else {
        output_static_edge_lines.join("\n")
    };
    let output_dynamic_edges = if output_dynamic_edge_lines.is_empty() {
        "- (none)".to_string()
    } else {
        output_dynamic_edge_lines.join("\n")
    };
    let output_static_cycle = output_static_cycle
        .map(|cycle| format_named_cycle(cycle, output_files, "chunk"))
        .unwrap_or_else(|| "- (none)".to_string());

    format!(
        concat!(
            "## Failed Seed\n",
            "`{seed}`\n\n",
            "## Structure\n",
            "- Nodes: `{node_count}`\n",
            "- Entry nodes: `{entry_nodes:?}`\n",
            "- CJS nodes: `{cjs_nodes:?}`\n",
            "- Preserve entry signatures: `{preserve_entry_signatures:?}`\n",
            "- Strict execution order: `{strict_execution_order:?}`\n",
            "- Minify internal exports: `{minify_internal_exports:?}`\n",
            "- External modules: `{external_module_count}`\n\n",
            "### Input Static Edges\n",
            "{input_static_edges}\n\n",
            "### Input Static Reexport Edges\n",
            "{input_reexport_edges}\n\n",
            "### Input Dynamic Edges\n",
            "{input_dynamic_edges}\n\n",
            "### Input External Dynamic Edges\n",
            "{input_external_dynamic_edges}\n\n",
            "### Output Files\n",
            "{output_files}\n\n",
            "### Output Static Edges\n",
            "{output_static_edges}\n\n",
            "### Output Static Cycle\n",
            "{output_static_cycle}\n\n",
            "### Output Dynamic Edges\n",
            "{output_dynamic_edges}\n\n",
            "### REPL URL\n",
            "{repl_url}\n\n",
            "### Generate Fixture\n",
            "```bash\n",
            "{fixture_command}\n",
            "```"
        ),
        seed = case.seed,
        node_count = case.node_count,
        entry_nodes = case.entry_nodes,
        cjs_nodes = case.cjs_nodes,
        preserve_entry_signatures = options.preserve_entry_signatures,
        strict_execution_order = options.strict_execution_order,
        minify_internal_exports = options.minify_internal_exports,
        external_module_count = case.external_module_count,
        input_static_edges = input_static_edges,
        input_reexport_edges = input_reexport_edges,
        input_dynamic_edges = input_dynamic_edges,
        input_external_dynamic_edges = input_external_dynamic_edges,
        output_files = output_files_markdown,
        output_static_edges = output_static_edges,
        output_static_cycle = output_static_cycle,
        output_dynamic_edges = output_dynamic_edges,
        repl_url = repl_url,
        fixture_command = fixture_command,
    )
}

#[cfg(test)]
mod cjs_cycle_tests {
    use super::{cycle_involves_cjs, graph_cycle_checker, is_immediate_cycle};

    /// Mirror of the runtime guard in [`super::run_case`]: a cycle is ignored only
    /// when it is immediate *and* involves a CommonJS chunk.
    fn is_ignored(cycle: &[usize], chunk_has_cjs: &[bool]) -> bool {
        is_immediate_cycle(cycle) && cycle_involves_cjs(cycle, chunk_has_cjs)
    }

    /// Output graph reported in
    /// <https://github.com/sapphi-red/zarara/issues/28#issuecomment-4619019145>:
    /// `entry-1.js` (index 0) and the `node1` chunk (index 2) directly import each
    /// other — an immediate cycle. `node1` is a CommonJS entry, so its chunk owns a
    /// `.cjs` module, reproducing the CJS↔ESM `ERR_REQUIRE_CYCLE_MODULE` boundary.
    #[test]
    fn reported_immediate_cjs_cycle_is_ignored() {
        // 0=entry-1.js, 1=node0 chunk, 2=node1 chunk, 3=node2 chunk
        let static_edges = vec![(0, 2), (1, 0), (1, 2), (2, 0), (3, 0)];
        let chunk_has_cjs = vec![false, true, true, false];

        let cycle = graph_cycle_checker::find_cycle(4, &static_edges)
            .expect("reported output graph is cyclic");

        assert!(
            is_immediate_cycle(&cycle),
            "reported cycle {cycle:?} is a two-chunk immediate cycle",
        );
        assert!(
            is_ignored(&cycle, &chunk_has_cjs),
            "immediate CJS cycle {cycle:?} must be ignored",
        );
    }

    /// The same immediate topology with no CommonJS modules is a genuine pure-ESM
    /// Rolldown output-graph defect and must still be reported.
    #[test]
    fn pure_esm_immediate_cycle_is_reported() {
        let static_edges = vec![(0, 2), (1, 0), (1, 2), (2, 0), (3, 0)];
        let chunk_has_cjs = vec![false, false, false, false];

        let cycle =
            graph_cycle_checker::find_cycle(4, &static_edges).expect("output graph is cyclic");

        assert!(
            !is_ignored(&cycle, &chunk_has_cjs),
            "pure-ESM cycle {cycle:?} must not be ignored",
        );
    }

    /// A larger (non-immediate) cycle is reportable even when it passes through a
    /// CommonJS chunk: only immediate CJS↔ESM cycles map to
    /// `ERR_REQUIRE_CYCLE_MODULE`.
    #[test]
    fn larger_cjs_cycle_is_reported() {
        // Cycle 0 -> 1 -> 2 -> 0 spans three chunks; chunk 1 owns a `.cjs` module.
        let static_edges = vec![(0, 1), (1, 2), (2, 0)];
        let chunk_has_cjs = vec![false, true, false];

        let cycle =
            graph_cycle_checker::find_cycle(3, &static_edges).expect("output graph is cyclic");

        assert!(
            !is_immediate_cycle(&cycle),
            "cycle {cycle:?} spans three chunks and is not immediate",
        );
        assert!(
            !is_ignored(&cycle, &chunk_has_cjs),
            "non-immediate cycle {cycle:?} must stay reported",
        );
    }

    /// A `.cjs`-owning chunk that is not part of the (immediate) cycle must not
    /// mask a genuine pure-ESM defect.
    #[test]
    fn cjs_chunk_outside_cycle_does_not_mask_defect() {
        // The cycle is 0 -> 1 -> 0; the CJS chunk (index 3) sits outside it.
        let static_edges = vec![(0, 1), (1, 0), (3, 0)];
        let chunk_has_cjs = vec![false, false, false, true];

        let cycle =
            graph_cycle_checker::find_cycle(4, &static_edges).expect("output graph is cyclic");

        assert!(
            !is_ignored(&cycle, &chunk_has_cjs),
            "cycle {cycle:?} avoids the CJS chunk and must stay reported",
        );
    }
}

fn format_named_cycle(cycle: &[usize], names: &[String], fallback_prefix: &str) -> String {
    if cycle.len() < 2 {
        return "- (none)".to_string();
    }

    let name_for = |index: usize| {
        names
            .get(index)
            .cloned()
            .unwrap_or_else(|| format!("{fallback_prefix}{index}"))
    };

    let path = cycle
        .iter()
        .map(|index| format!("`{}`", name_for(*index)))
        .collect::<Vec<_>>()
        .join(" -> ");
    format!("- Path: {path}")
}
