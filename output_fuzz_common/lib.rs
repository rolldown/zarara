#![cfg_attr(not(test), allow(dead_code, unused_imports))]

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as BASE64_STANDARD;
use flate2::Compression;
use flate2::write::ZlibEncoder;
use proptest::prelude::*;
use rolldown::{
    Bundler, BundlerOptions, InputItem, IsExternal, OutputFormat, PreserveEntrySignatures,
    TreeshakeOptions,
};
use rolldown_common::Output;
use std::collections::HashSet;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

pub const MAX_NODES: usize = 12;
pub const MAX_EXTERNAL_MODULES: usize = 4;

/// Number of distinct local-name variants used for static default imports from
/// the same external module. The bug in
/// <https://github.com/rolldown/rolldown/issues/9754> only fires when two or
/// more importing modules pick *different* local names for the same external
/// default; with multiple variants in play the fuzz strategy lands on that
/// shape frequently.
pub const EXTERNAL_DEFAULT_LOCAL_VARIANTS: u8 = 4;

pub fn external_module_name(index: usize) -> String {
    format!("external-{index}")
}

pub fn external_default_local_name(external_index: usize, variant: u8) -> String {
    format!("dn_{external_index}_{variant}")
}

#[derive(Clone)]
pub struct GraphCase {
    pub seed: u64,
    pub node_count: usize,
    pub entry_nodes: Vec<usize>,
    pub cjs_nodes: Vec<usize>,
    pub static_edges: Vec<(usize, usize)>,
    pub dynamic_edges: Vec<(usize, usize)>,
    pub reexport_static_edges: Vec<(usize, usize)>,
    pub external_module_count: usize,
    pub external_dynamic_edges: Vec<(usize, usize)>,
    /// Static `import <local> from "<external>"` edges, where `<local>` is
    /// drawn from a small per-external pool of variants (see
    /// [`EXTERNAL_DEFAULT_LOCAL_VARIANTS`]). When two modules import the same
    /// external default under different variants, rolldown must merge them in
    /// the output — historically nondeterministically (see #9754).
    /// Each entry is `(from_node, external_index, variant)`.
    pub external_static_default_edges: Vec<(usize, usize, u8)>,
    pub preserve_entry_signatures: PreserveEntrySignatures,
    pub strict_execution_order: bool,
    pub treeshake: bool,
    pub minify_internal_exports: bool,
}

impl std::fmt::Debug for GraphCase {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("GraphCase")
            .field("seed", &self.seed)
            .finish()
    }
}

pub fn acyclic_graph_case_strategy() -> impl Strategy<Value = GraphCase> {
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
            |(seed, node_count, preserve_entry_signatures_index, strict_execution_order, treeshake, minify_internal_exports, external_module_count)| {
                let static_edge_slots = node_count * (node_count - 1) / 2;
                let dynamic_edge_slots = node_count * (node_count - 1);
                let external_dynamic_slots = node_count * external_module_count;
                let external_static_default_slots = node_count * external_module_count;
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
                        prop::collection::vec(
                            0u8..=EXTERNAL_DEFAULT_LOCAL_VARIANTS,
                            external_static_default_slots,
                        ),
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
                (dynamic_mask, reexport_mask, external_dynamic_mask, external_static_default_mask),
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
                    &external_static_default_mask,
                )
            },
        )
}

pub fn preserve_entry_signatures_from_index(index: u8) -> PreserveEntrySignatures {
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
    external_static_default_mask: &[u8],
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

    let mut external_static_default_edges = Vec::new();
    let mut ext_static_idx = 0;
    for from in 0..node_count {
        for ext in 0..external_module_count {
            let raw = external_static_default_mask
                .get(ext_static_idx)
                .copied()
                .unwrap_or(0);
            if raw > 0 && raw <= EXTERNAL_DEFAULT_LOCAL_VARIANTS {
                external_static_default_edges.push((from, ext, raw - 1));
            }
            ext_static_idx += 1;
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
        external_static_default_edges,
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

    let external_static_default_slots = node_count * external_module_count;
    let external_static_default_mask = (0..external_static_default_slots)
        .map(|_| (rng.next_u64() % u64::from(EXTERNAL_DEFAULT_LOCAL_VARIANTS + 1)) as u8)
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
        &external_static_default_mask,
    )
}

/// Number of bundles run per case in [`run_deterministic_check`]. Some
/// nondeterminism (e.g. rolldown#9754) only flips a small fraction of builds,
/// so each case needs several independent bundles before it can reliably catch
/// the divergence at the default `PROPTEST_CASES` count.
const DETERMINISTIC_CHECK_ITERATIONS: usize = 4;

pub async fn run_deterministic_check(case: GraphCase) -> Result<(), String> {
    let fixture_dir = create_fixture_dir(case.seed).map_err(|err| err.to_string())?;
    materialize_graph_modules(&fixture_dir, &case).map_err(|err| err.to_string())?;
    let options = bundler_options_for_case(&case, fixture_dir.clone());

    let baseline = {
        let mut bundler = Bundler::new(options.clone()).map_err(|err| err.to_string())?;
        let output = bundler.generate().await.map_err(|err| err.to_string())?;
        collect_chunk_info(&output)
    };

    for iteration in 1..DETERMINISTIC_CHECK_ITERATIONS {
        let chunks = {
            let mut bundler = Bundler::new(options.clone()).map_err(|err| err.to_string())?;
            let output = bundler.generate().await.map_err(|err| err.to_string())?;
            collect_chunk_info(&output)
        };

        if let Err(why) = compare_chunk_info(&baseline, &chunks, case.seed, iteration) {
            std::fs::remove_dir_all(&fixture_dir).ok();
            return Err(why);
        }
    }

    std::fs::remove_dir_all(&fixture_dir).map_err(|err| err.to_string())?;
    Ok(())
}

fn compare_chunk_info(
    baseline: &[ChunkInfo],
    chunks: &[ChunkInfo],
    seed: u64,
    iteration: usize,
) -> Result<(), String> {
    if baseline.len() != chunks.len() {
        return Err(format!(
            "Nondeterministic output: baseline produced {} chunks, iteration {iteration} produced {} chunks (seed {seed})",
            baseline.len(),
            chunks.len(),
        ));
    }

    for (a, b) in baseline.iter().zip(chunks.iter()) {
        if a.filename != b.filename {
            return Err(format!(
                "Nondeterministic output: chunk filenames differ at iteration {iteration}: '{}' vs '{}' (seed {seed})",
                a.filename, b.filename,
            ));
        }
        if a.code != b.code {
            return Err(format!(
                "Nondeterministic output: code differs for chunk '{}' at iteration {iteration} (seed {seed})",
                a.filename,
            ));
        }
        if a.imports != b.imports {
            return Err(format!(
                "Nondeterministic output: imports differ for chunk '{}' at iteration {iteration} (seed {seed})",
                a.filename,
            ));
        }
        if a.exports != b.exports {
            return Err(format!(
                "Nondeterministic output: exports differ for chunk '{}' at iteration {iteration} (seed {seed})",
                a.filename,
            ));
        }
        if a.dynamic_imports != b.dynamic_imports {
            return Err(format!(
                "Nondeterministic output: dynamic_imports differ for chunk '{}' at iteration {iteration} (seed {seed})",
                a.filename,
            ));
        }
    }

    Ok(())
}

pub struct ChunkInfo {
    pub filename: String,
    pub code: String,
    pub imports: Vec<String>,
    pub exports: Vec<String>,
    pub dynamic_imports: Vec<String>,
}

pub fn collect_chunk_info(output: &rolldown::BundleOutput) -> Vec<ChunkInfo> {
    let mut chunks: Vec<ChunkInfo> = output
        .assets
        .iter()
        .filter_map(|asset| match asset {
            Output::Chunk(chunk) => Some(ChunkInfo {
                filename: chunk.filename.to_string(),
                code: chunk.code.clone(),
                imports: chunk.imports.iter().map(|s| s.to_string()).collect(),
                exports: chunk.exports.iter().map(|s| s.to_string()).collect(),
                dynamic_imports: chunk.dynamic_imports.iter().map(|s| s.to_string()).collect(),
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

pub fn bundler_options_for_case(case: &GraphCase, cwd: PathBuf) -> BundlerOptions {
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

pub fn is_cjs_node(case: &GraphCase, index: usize) -> bool {
    case.cjs_nodes.binary_search(&index).is_ok()
}

pub fn module_filename(case: &GraphCase, index: usize) -> String {
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
    let mut external_static_default_outgoing =
        vec![Vec::<(usize, u8)>::new(); case.node_count];
    for &(from, ext_index, variant) in &case.external_static_default_edges {
        external_static_default_outgoing[from].push((ext_index, variant));
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
                source.push_str(&format!(
                    "use_{from}_{destination}.ref = {from};\n"
                ));
                source.push_str(&format!(
                    "export {{ use_{from}_{destination} }};\n"
                ));
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
        // Static default imports from external modules. CJS modules can't
        // express ES default imports, so we just emit a `require` with a
        // deterministic local name (no variant) for them — the #9754 bug only
        // arises in the ESM-merge path anyway. ESM modules emit the variant
        // local name and reference it on `globalThis` so the import survives
        // tree-shaking.
        for (ext_index, variant) in &external_static_default_outgoing[from] {
            let ext_name = external_module_name(*ext_index);
            if current_file_is_cjs {
                source.push_str(&format!(
                    "const dn_cjs_{from}_{ext_index} = require(\"{ext_name}\");\n"
                ));
                source.push_str(&format!(
                    "exports.use_dn_{from}_{ext_index} = dn_cjs_{from}_{ext_index};\n"
                ));
            } else {
                let local = external_default_local_name(*ext_index, *variant);
                source.push_str(&format!("import {local} from \"{ext_name}\";\n"));
                source.push_str(&format!(
                    "globalThis.__use_dn_{from}_{ext_index}_{variant} = {local};\n"
                ));
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

pub fn materialize_graph_modules(dir: &Path, case: &GraphCase) -> std::io::Result<()> {
    for (filename, source) in render_graph_modules(case) {
        std::fs::write(dir.join(filename), source)?;
    }
    Ok(())
}

pub fn create_fixture_dir(seed: u64) -> std::io::Result<PathBuf> {
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

fn encode_triplet_list(triplets: &[(usize, usize, u8)]) -> String {
    if triplets.is_empty() {
        "none".to_string()
    } else {
        triplets
            .iter()
            .map(|(a, b, c)| format!("{a}-{b}-{c}"))
            .collect::<Vec<_>>()
            .join(",")
    }
}

fn decode_triplet_list(value: &str) -> Result<Vec<(usize, usize, u8)>, String> {
    if value.is_empty() || value == "none" {
        return Ok(Vec::new());
    }
    value
        .split(',')
        .map(|part| {
            let mut iter = part.split('-');
            let a = iter
                .next()
                .ok_or_else(|| format!("invalid triplet `{part}`"))?
                .parse::<usize>()
                .map_err(|_| format!("invalid triplet field in `{part}`"))?;
            let b = iter
                .next()
                .ok_or_else(|| format!("invalid triplet `{part}`"))?
                .parse::<usize>()
                .map_err(|_| format!("invalid triplet field in `{part}`"))?;
            let c = iter
                .next()
                .ok_or_else(|| format!("invalid triplet `{part}`"))?
                .parse::<u8>()
                .map_err(|_| format!("invalid triplet field in `{part}`"))?;
            if iter.next().is_some() {
                return Err(format!("invalid triplet `{part}`"));
            }
            Ok((a, b, c))
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

pub fn encode_case_spec(case: &GraphCase) -> String {
    format!(
        "n={n};e={e};c={c};s={s};d={d};r={r};x={x};xs={xs};p={p};o={o};t={t};m={m}",
        n = case.node_count,
        e = encode_node_list(&case.entry_nodes),
        c = encode_node_list(&case.cjs_nodes),
        s = encode_edge_list(&case.static_edges),
        d = encode_edge_list(&case.dynamic_edges),
        r = encode_edge_list(&case.reexport_static_edges),
        x = encode_external_spec(case.external_module_count, &case.external_dynamic_edges),
        xs = encode_triplet_list(&case.external_static_default_edges),
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
    let mut external_static_default_triplets: Option<Vec<(usize, usize, u8)>> = None;
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
            "xs" => {
                external_static_default_triplets = Some(decode_triplet_list(value)?);
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
    let (external_module_count, external_dynamic_edges) =
        external_spec.unwrap_or((0, Vec::new()));
    let external_dynamic_edges = {
        let mut edges = external_dynamic_edges;
        edges.retain(|(from, ext)| *from < node_count && *ext < external_module_count);
        edges.sort_unstable();
        edges
    };
    let external_static_default_edges = {
        let mut triplets = external_static_default_triplets.unwrap_or_default();
        triplets.retain(|(from, ext, variant)| {
            *from < node_count
                && *ext < external_module_count
                && *variant < EXTERNAL_DEFAULT_LOCAL_VARIANTS
        });
        // Dedup `(from, ext)`: one variant per (node, external) pair.
        let mut seen = HashSet::new();
        triplets.retain(|(from, ext, _)| seen.insert((*from, *ext)));
        triplets.sort_unstable();
        triplets
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
        external_static_default_edges,
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

pub fn build_repl_url(case: &GraphCase, options: &BundlerOptions) -> String {
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

