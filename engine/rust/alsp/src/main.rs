use std::path::PathBuf;

use alsp::{build_java_repo_map, lookup_java_symbol};

fn main() -> anyhow::Result<()> {
    let args: Vec<String> = std::env::args().collect();
    if args.len() >= 2 && args[1] == "lookup" {
        if args.len() < 4 {
            anyhow::bail!("usage: alsp lookup <root> <symbol>");
        }
        let root = PathBuf::from(&args[2]);
        let symbol = &args[3];
        let hit = lookup_java_symbol(&root, symbol)?;
        println!("{}", serde_json::to_string_pretty(&hit)?);
        return Ok(());
    }

    let root = args
        .get(1)
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."));
    let map = build_java_repo_map(&root)?;
    println!("{}", serde_json::to_string_pretty(&map)?);
    Ok(())
}
