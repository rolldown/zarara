use std::path::PathBuf;

fn main() -> Result<(), String> {
    let mut seed = None;
    let mut case = None;
    let mut out = None;

    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--seed" => {
                let value = args
                    .next()
                    .ok_or_else(|| "--seed requires a value".to_string())?;
                let parsed = value
                    .parse::<u64>()
                    .map_err(|_| "--seed must be a u64".to_string())?;
                seed = Some(parsed);
            }
            "--out" => {
                let value = args
                    .next()
                    .ok_or_else(|| "--out requires a value".to_string())?;
                out = Some(PathBuf::from(value));
            }
            "--case" => {
                let value = args
                    .next()
                    .ok_or_else(|| "--case requires a value".to_string())?;
                case = Some(value);
            }
            "--help" | "-h" => {
                print_help();
                return Ok(());
            }
            other => {
                return Err(format!("unknown argument: {other}"));
            }
        }
    }

    let seed = seed.ok_or_else(|| "missing required --seed <u64>".to_string())?;
    let dir = if let Some(case_spec) = case {
        output_fuzz_common::generate_fixture_from_case_spec(seed, &case_spec, out)
            .map_err(|err| format!("failed to generate fixture from --case: {err}"))?
    } else {
        output_fuzz_common::generate_fixture_from_seed(seed, out)
            .map_err(|err| format!("failed to generate fixture from --seed: {err}"))?
    };

    println!("{}", dir.display());
    Ok(())
}

fn print_help() {
    println!(
        "Usage: cargo run -p acyclic_output_fuzz --bin generate_fixture -- --seed <u64> [--case <spec>] [--out <dir>]"
    );
}
