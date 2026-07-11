use std::{
    env, fs,
    io::{self, Write},
    path::{Path, PathBuf},
};

fn main() -> io::Result<()> {
    let manifest_dir =
        PathBuf::from(env::var_os("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR"));
    let workspace_dir = manifest_dir
        .parent()
        .and_then(Path::parent)
        .expect("control-api is under apps/control-api");
    let out_dir = PathBuf::from(env::var_os("OUT_DIR").expect("OUT_DIR"));
    let generated = out_dir.join("web_assets.rs");

    println!("cargo:rerun-if-env-changed=HALOLAKE_WEB_BUILD_ID");

    let default_dist = workspace_dir.join("web/new-api/default/dist");
    let classic_dist = workspace_dir.join("web/new-api/classic/dist");

    let default_assets = collect_assets(&default_dist)?;
    let classic_assets = collect_assets(&classic_dist)?;

    let mut file = fs::File::create(generated)?;
    write_assets(&mut file, "DEFAULT_WEB_ASSETS", &default_assets)?;
    write_assets(&mut file, "CLASSIC_WEB_ASSETS", &classic_assets)?;
    Ok(())
}

fn collect_assets(root: &Path) -> io::Result<Vec<(String, PathBuf)>> {
    println!("cargo:rerun-if-changed={}", root.display());

    let mut assets = Vec::new();
    if root.is_dir() {
        collect_assets_inner(root, root, &mut assets)?;
    }
    assets.sort_by(|a, b| a.0.cmp(&b.0));
    Ok(assets)
}

fn collect_assets_inner(
    root: &Path,
    current: &Path,
    assets: &mut Vec<(String, PathBuf)>,
) -> io::Result<()> {
    let mut entries = fs::read_dir(current)?.collect::<Result<Vec<_>, _>>()?;
    entries.sort_by_key(|entry| entry.path());

    for entry in entries {
        let path = entry.path();
        if path.is_dir() {
            println!("cargo:rerun-if-changed={}", path.display());
            collect_assets_inner(root, &path, assets)?;
        } else if path.is_file() {
            println!("cargo:rerun-if-changed={}", path.display());
            let relative = path
                .strip_prefix(root)
                .expect("asset is under dist root")
                .to_string_lossy()
                .replace('\\', "/");
            assets.push((relative, path));
        }
    }
    Ok(())
}

fn write_assets(file: &mut fs::File, name: &str, assets: &[(String, PathBuf)]) -> io::Result<()> {
    writeln!(file, "pub(crate) static {name}: &[EmbeddedAsset] = &[")?;
    for (relative, path) in assets {
        writeln!(
            file,
            "    EmbeddedAsset {{ path: {}, bytes: include_bytes!({}) }},",
            rust_string_literal(relative),
            rust_string_literal(&path.to_string_lossy()),
        )?;
    }
    writeln!(file, "];")?;
    Ok(())
}

fn rust_string_literal(value: &str) -> String {
    format!("{value:?}")
}
