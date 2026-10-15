use std::{error::Error, path::PathBuf};

fn main() {
    if let Err(error) = run() {
        eprintln!("suede-artwork: {error}");
        std::process::exit(1);
    }
}

fn run() -> Result<(), Box<dyn Error>> {
    let mut args = std::env::args_os().skip(1);
    let mode = args.next().and_then(|value| value.into_string().ok());
    let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let source_dir = manifest_dir.join("../../artwork");
    let assets = match mode.as_deref() {
        Some("docs") if args.next().is_none() => {
            let output_dir = manifest_dir.join("../../docs/generated/artwork");
            suede_artwork::generate_docs(&source_dir, &output_dir)?
        }
        Some("web") => {
            let output_dir = args
                .next()
                .map(PathBuf::from)
                .ok_or("usage: cargo run -p suede-artwork -- web OUTPUT_DIR")?;
            if args.next().is_some() {
                return Err("usage: cargo run -p suede-artwork -- web OUTPUT_DIR".into());
            }
            suede_artwork::generate_web(&source_dir, &output_dir)?
        }
        _ => return Err("usage: cargo run -p suede-artwork -- docs | web OUTPUT_DIR".into()),
    };
    println!("Generated {} artwork assets.", assets.len());
    Ok(())
}
