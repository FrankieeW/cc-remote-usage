//! Isolated verification helper; never selects the user's database implicitly.
use std::path::Path;
fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<_> = std::env::args().collect();
    if args.len() != 4 {
        return Err("usage: sandbox_import DATABASE codex|claude MIRROR".into());
    }
    let result = ccr_importer::import(Path::new(&args[1]), &args[2], Path::new(&args[3]))?;
    println!("{}", serde_json::to_string(&result)?);
    if !result.errors.is_empty() {
        return Err("file imports failed".into());
    }
    Ok(())
}
