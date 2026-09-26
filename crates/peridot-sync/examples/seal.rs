//! Seal a file the way `peridot share` does, for trying the viewer page
//! locally: writes the encrypted blob as `<out dir>/<sha256>` and prints
//! the link fragment for a server at `<host:port>`.
//!
//!   cargo run -p peridot-sync --example seal -- <file> <out dir> <host:port>

use peridot_sync::share::{Link, mime_for, seal};

fn main() -> anyhow::Result<()> {
    let args: Vec<String> = std::env::args().collect();
    let [_, file, out, server] = args.as_slice() else {
        anyhow::bail!("usage: seal <file> <out dir> <host:port>");
    };
    let name = std::path::Path::new(file)
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "file".into());
    let data = std::fs::read(file)?;
    let sealed = seal(&name, mime_for(&name), &data)?;
    std::fs::create_dir_all(out)?;
    std::fs::write(format!("{out}/{}", sealed.sha256), &sealed.blob)?;
    let link = Link {
        sha256: sealed.sha256.clone(),
        server: server.clone(),
        key: *sealed.key,
    };
    println!("{}", link.to_url("http://127.0.0.1:8765/s/"));
    Ok(())
}
