# zarara

This repository is a focused fuzzing harness for Rolldown output graph behavior.

It generates random module graphs, bundles them with Rolldown, and asserts:

- input **static** graph is acyclic
- output **static chunk-import** graph is also acyclic
- output chunks contain **valid JavaScript** (parsed with `oxc_parser`)
- entry chunks **preserve all expected exports** (when `preserveEntrySignatures: "strict"`)
- output is **deterministic** (bundling twice produces identical results)

When a failure is found, the test prints markdown with:

- failed seed
- minimized graph structure
- detected output cycle path
- REPL URL (`repl.rolldown.rs`) for quick inspection
- fixture generation command

## Repository Layout

- `output_fuzz_common/`
  - Shared fuzz infrastructure (`GraphCase`, materialization, case-spec
    encoding, `generate_fixture` binary)
- `acyclic_output_fuzz/`
  - Acyclic-input/acyclic-output property test + cycle checker
- `deterministic_output_fuzz/`
  - Determinism property test (covers [rolldown#9754](https://github.com/rolldown/rolldown/issues/9754))
- `rolldown/`
  - Rolldown source as a submodule dependency for the harness
- `.github/workflows/output_fuzz.yml`
  - scheduled fuzz workflow + issue tracking

## Fuzz Tests

| Test | What it checks |
|------|---------------|
| `acyclic_input_produces_acyclic_output` | Acyclic input graphs produce acyclic output chunk-import graphs, valid JS, entry export preservation. |
| `deterministic_output` | Bundling the same graph twice produces identical chunks (filenames, code, imports, exports). Covers [rolldown#9754](https://github.com/rolldown/rolldown/issues/9754) via static default imports from the same external module under different local names per importer. |

## Run Locally

Run all fuzz tests:

```bash
cargo test --workspace -- --nocapture
```

Run a specific test:

```bash
cargo test -p acyclic_output_fuzz acyclic_input_produces_acyclic_output -- --nocapture
cargo test -p deterministic_output_fuzz deterministic_output -- --nocapture
```

Increase search space:

```bash
PROPTEST_CASES=2000 cargo test --workspace -- --nocapture
```

Use a deterministic RNG seed:

```bash
PROPTEST_RNG_SEED=123456 cargo test --workspace -- --nocapture
```

## Reproduce a Failure

Use the command printed in `### Generate Fixture`, for example:

```bash
cargo run -p output_fuzz_common --bin generate_fixture -- --seed <u64> --case '<case-spec>' --out ./fixtures/seed-<u64>
```

That writes:

- generated input modules (`node*.js`)
- `rolldown.config.js` with matching fuzz options

You can also open the printed `### REPL URL` directly in `https://repl.rolldown.rs/`.

## CI Workflow

The workflow in `.github/workflows/output_fuzz.yml`:

- runs daily and on manual dispatch
- uploads the fuzz log artifact
- creates/updates a tracking issue in the current repository on failure
- posts the exact failure markdown as an issue comment
