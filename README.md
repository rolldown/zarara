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

- `acyclic_output_fuzz/`
  - Rust crate with the property test and fixture generator
- `rolldown/`
  - Rolldown source as a submodule dependency for the harness
- `.github/workflows/acyclic_output_fuzz.yml`
  - scheduled fuzz workflow + issue tracking

## Fuzz Tests

| Test | What it checks |
|------|---------------|
| `acyclic_input_produces_acyclic_output` | Acyclic input graphs produce acyclic output chunk-import graphs, valid JS, entry export preservation. |
| `deterministic_output` | Bundling the same graph twice produces identical chunks (filenames, code, imports, exports). Covers [rolldown#9754](https://github.com/rolldown/rolldown/issues/9754) via static default imports from the same external module under different local names per importer. |

## Run Locally

Run all fuzz tests:

```bash
cargo test -p acyclic_output_fuzz -- --nocapture
```

Run a specific test:

```bash
cargo test -p acyclic_output_fuzz acyclic_input_produces_acyclic_output -- --nocapture
```

Increase search space:

```bash
PROPTEST_CASES=2000 cargo test -p acyclic_output_fuzz -- --nocapture
```

Use a deterministic RNG seed:

```bash
PROPTEST_RNG_SEED=123456 cargo test -p acyclic_output_fuzz -- --nocapture
```

## Reproduce a Failure

Use the command printed in `### Generate Fixture`, for example:

```bash
cargo run -p acyclic_output_fuzz --bin generate_fixture -- --seed <u64> --case '<case-spec>' --out ./fixtures/seed-<u64>
```

That writes:

- generated input modules (`node*.js`)
- `rolldown.config.js` with matching fuzz options

You can also open the printed `### REPL URL` directly in `https://repl.rolldown.rs/`.

## CI Workflow

The workflow in `.github/workflows/acyclic_output_fuzz.yml`:

- runs daily and on manual dispatch
- uploads the fuzz log artifact
- creates/updates a tracking issue in the current repository on failure
- posts the exact failure markdown as an issue comment
