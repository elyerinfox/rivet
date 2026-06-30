//! Implementation of `rivet serve` (HTTP transcode API server).

use anyhow::{Context, Result};

pub(crate) fn run(addr: String) -> Result<()> {
    let addr: std::net::SocketAddr = addr.parse().context("parsing --addr")?;
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .context("building tokio runtime")?;
    eprintln!("rivet transcode API on http://{addr} (POST media to /v1/transcode)");
    rt.block_on(rivet::server::serve(addr))
}
