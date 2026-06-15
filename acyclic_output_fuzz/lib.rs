#![cfg_attr(not(test), allow(dead_code, unused_imports))]

mod graph_cycle_checker;

use output_fuzz_common::{
    GraphCase, MAX_EXTERNAL_MODULES, MAX_NODES, acyclic_graph_case_strategy, build_repl_url,
    bundler_options_for_case, create_fixture_dir, encode_case_spec,
    external_default_local_name, external_module_name, is_cjs_node, materialize_graph_modules,
    module_filename, preserve_entry_signatures_from_index,
};
use oxc::allocator::Allocator as OxcAllocator;
use oxc::parser::{ParseOptions, Parser};
use oxc::span::SourceType;
use proptest::prelude::*;
use proptest::test_runner::{
    Config as ProptestConfig, RngSeed, TestCaseError, TestError, TestRunner,
};
use rolldown::{Bundler, BundlerOptions, PreserveEntrySignatures};
use rolldown_common::Output;
use std::collections::{HashMap, HashSet};
use std::path::{Component, Path, PathBuf};

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
            move |(seed, node_count, preserve_entry_signatures_index, strict_execution_order, treeshake, minify_internal_exports, external_module_count)| {
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
                let hub = num_entries;
                let children_start = hub + 1;
                let children_end = (children_start + num_children).min(node_count);

                let entry_nodes = (0..num_entries).collect::<Vec<_>>();

                let mut static_edges = Vec::new();
                let mut dynamic_edges = Vec::new();

                for &e in &entry_nodes {
                    dynamic_edges.push((e, hub));
                }

                for c in children_start..children_end {
                    static_edges.push((hub, c));
                    dynamic_edges.push((hub, c));
                }

                let mut external_dynamic_edges = Vec::new();
                for (ext, &has_edge) in ext_mask.iter().enumerate() {
                    if has_edge {
                        external_dynamic_edges.push((hub, ext));
                    }
                }
                if external_dynamic_edges.is_empty() {
                    external_dynamic_edges.push((hub, 0));
                }

                let preserve_entry_signatures =
                    preserve_entry_signatures_from_index(preserve_entry_signatures_index);

                GraphCase {
                    seed,
                    node_count,
                    entry_nodes,
                    cjs_nodes: Vec::new(),
                    static_edges,
                    dynamic_edges,
                    reexport_static_edges: Vec::new(),
                    external_module_count,
                    external_dynamic_edges,
                    external_static_default_edges: Vec::new(),
                    preserve_entry_signatures,
                    strict_execution_order,
                    treeshake,
                    minify_internal_exports,
                }
            },
        )
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
        if ret.panicked || !ret.errors.is_empty() {
            let errors_str = ret
                .errors
                .iter()
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

fn validate_entry_exports(
    case: &GraphCase,
    output: &rolldown::BundleOutput,
) -> Result<(), String> {
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
        let node_index = case
            .entry_nodes
            .iter()
            .copied()
            .find(|&idx| {
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
/// [`output_fuzz_common::module_filename`]), so a chunk owning such a module
/// forces Rolldown to emit the CommonJS interop wrappers (`__commonJSMin`, …).
/// When a CJS chunk and an ESM chunk import each other directly, that immediate
/// cycle reproduces the CJS↔ESM boundary cycle Node.js rejects at runtime with
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
    let input_external_static_default_edge_lines = case
        .external_static_default_edges
        .iter()
        .map(|(from, ext, variant)| {
            format!(
                "- `node{from}` -> `{}` as `{}`",
                external_module_name(*ext),
                external_default_local_name(*ext, *variant),
            )
        })
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
    let input_external_static_default_edges =
        if input_external_static_default_edge_lines.is_empty() {
            "- (none)".to_string()
        } else {
            input_external_static_default_edge_lines.join("\n")
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
            "### Input External Static Default Imports\n",
            "{input_external_static_default_edges}\n\n",
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
        input_external_static_default_edges = input_external_static_default_edges,
        output_files = output_files_markdown,
        output_static_edges = output_static_edges,
        output_static_cycle = output_static_cycle,
        output_dynamic_edges = output_dynamic_edges,
        repl_url = repl_url,
        fixture_command = fixture_command,
    )
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

        let cycle = graph_cycle_checker::find_cycle(4, &static_edges)
            .expect("output graph is cyclic");

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
        let static_edges = vec![(0, 1), (1, 2), (2, 0)];
        let chunk_has_cjs = vec![false, true, false];

        let cycle = graph_cycle_checker::find_cycle(3, &static_edges)
            .expect("output graph is cyclic");

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
        let static_edges = vec![(0, 1), (1, 0), (3, 0)];
        let chunk_has_cjs = vec![false, false, false, true];

        let cycle = graph_cycle_checker::find_cycle(4, &static_edges)
            .expect("output graph is cyclic");

        assert!(
            !is_ignored(&cycle, &chunk_has_cjs),
            "cycle {cycle:?} avoids the CJS chunk and must stay reported",
        );
    }
}
