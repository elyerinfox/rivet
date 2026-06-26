//! NVENC capability-query validation (real `nvEncGetEncodeCaps` / `GetEncodeGUIDs`
//! driver query, not a board-name guess). Requires the `nvidia` feature.
//!
//! On a GPU whose NVENC lacks AV1 (Ampere consumer like the RTX 3090, Turing,
//! Pascal) the encoder must bail with a clean "no AV1 encode" message instead of
//! crashing or silently producing garbage. On Ada+ it constructs successfully.
#![cfg(feature = "nvidia")]

use codec::encode::EncoderConfig;
use codec::encode::nvenc::NvencEncoder;

#[test]
fn nvenc_capability_query_validates_av1_support() {
    let cfg = EncoderConfig {
        width: 1280,
        height: 720,
        ..Default::default()
    };
    match NvencEncoder::new(cfg, 0) {
        Ok(_) => {
            // GPU 0's NVENC advertises AV1 (Ada+ / Ampere datacenter) — the
            // capability query passed and the encoder initialized.
            eprintln!("NVENC AV1 capability validated on GPU 0 (Ada+/datacenter)");
        }
        Err(e) => {
            let msg = e.to_string();
            // No usable NVIDIA NVENC on GPU 0 (no driver / no GPU / session
            // open failed) — nothing for this test to assert; skip.
            if msg.contains("cuInit")
                || msg.contains("cuDevice")
                || msg.contains("cuCtx")
                || msg.contains("OpenEncodeSession")
                || msg.contains("libnvidia")
                || msg.contains("NvEncodeAPI")
                || msg.contains("fn-list")
            {
                eprintln!("skip: no usable NVENC on GPU 0 ({msg})");
                return;
            }
            // Otherwise the only way `new` errors is the capability query
            // rejecting the request — exactly what we're validating. On the
            // RTX 3090 this is the "does not support AV1 encode" path.
            assert!(
                msg.contains("AV1") || msg.contains("10-bit") || msg.contains("maxes at"),
                "expected an NVENC capability rejection, got: {msg}"
            );
            eprintln!("NVENC capability query rejected cleanly: {msg}");
        }
    }
}
