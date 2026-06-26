# rivet — TODO / hardware-verification backlog

## AMD (AMF) + Intel (QSV) hardware **decode** — verify on real silicon

Status: **implemented as hand-rolled FFI, verified-by-review only.** Neither was
testable on the dev box (RTX 3090 + Ryzen iGPU — no AMD RDNA3+ discrete, no Intel
Arc). Both are our own FFI mirrors of the vendor SDK headers (no shiguredo code,
no attribution owed), modeled on the in-tree encoders (`encode/amf.rs`,
`encode/qsv.rs`) + the AMD AMF / Intel oneVPL decode APIs.

When the **Intel Arc** and **AMD RDNA-class** cards arrive, verify:

### AMD — `decode/amf_dec.rs` (AMF decode)
- [ ] H.264 / HEVC / AV1 decode produces correct pixels (luma spread non-flat;
      compare a frame hash against ffmpeg).
- [ ] **`AMF_IID_SURFACE` GUID** — `QueryOutput` returns `AMFData`; we downcast
      to `AMFSurface` via `QueryInterface(AMF_IID_SURFACE)`. The GUID bytes are a
      **best guess** from the AMF SDK `core/Surface.h` — confirm against the
      installed SDK header; a wrong IID makes every `QueryOutput` fail.
- [ ] **Extradata / SPS-PPS**: H.264/HEVC AMF decoders may need
      `AMF_VIDEO_DECODER_EXTRADATA` set before `Init`. We currently rely on
      in-band parameter sets (Annex-B). Confirm whether MP4-sourced streams
      (which carry SPS/PPS out-of-band) need the extradata property set.
- [ ] **P010 / 10-bit** output path (HEVC Main10) → `Yuv420p10le` deinterleave.
- [ ] Drain (`Drain` + `QueryOutput` until `AMF_EOF`) flushes all frames.
- [ ] Multi-AMD adapter routing (AMF init picks adapter 0 unconditionally).

### Intel — `decode/qsv_dec.rs` (oneVPL decode)
- [ ] H.264 / HEVC / AV1 / VP9 decode produces correct pixels.
- [ ] **`MFXVideoDECODE_DecodeHeader`** correctly parses the bitstream header
      into `mfxVideoParam` (we feed the first sample(s) and retry on
      `MFX_ERR_MORE_DATA`).
- [ ] Work-surface pool sizing from `MFXVideoDECODE_QueryIOSurf`
      (`Suggested` count) — we currently allocate a fixed pool; confirm it's
      enough for the stream's DPB depth.
- [ ] **P010 / 10-bit** output (HEVC Main10 / VP9 Profile 2) with the `Shift`
      handling on read-back.
- [ ] Drain (null bitstream `DecodeFrameAsync` until `MFX_ERR_MORE_DATA`).
- [ ] DRM render-node selection on multi-Intel hosts (the hand-rolled QSV
      encoder picks the implementation via the dispatcher; decode should match).

### Both
- [ ] Wire into `create_decoder` dispatch (done — AMD/Intel branches restored,
      gated behind `amd` / `qsv`).
- [ ] Confirm `cargo build --features amd` / `--features qsv` on a Linux host
      with the vendor runtime present, then end-to-end decode→AV1-encode.
- [ ] If a path proves unreliable, the `ffmpeg` decode feature remains the
      fallback for that vendor.

## AV1 **encode** — verify on AV1-encode silicon

Status: **fully implemented + building** (all three vendors hand-rolled in-tree,
Windows + Linux: `encode/nvenc.rs`, `encode/amf.rs`, `encode/qsv.rs`; 8-bit NV12
+ 10-bit P010; CQP / VBR / ICQ; ChunkSeamMode constant-QP; HDR signalling).
**No functional gaps** — the only open items are hardware verification, because
the dev box (RTX 3090 Ampere) has no AV1-encode silicon and there's no AMD
RDNA3+ / Intel Arc here. This is the *encode* counterpart of the decode backlog
above; the same Intel + AMD cards (plus an Ada+ NVIDIA card) cover it.

### NVENC (NVIDIA, Ada+)
- Capability query is **hardware-proven** on the 3090 (correctly rejects AV1 —
  "2 codecs, none AV1"). The rest is verified-by-review:
- [ ] End-to-end AV1 encode on Ada+ (RTX 4000+ / A10G / L4 / Blackwell): correct
      pixels (decode the output, compare against the source), valid `av1C`.
- [ ] 10-bit P010 encode path (HDR10 ramp → `ffprobe pix_fmt=yuv420p10le` + the
      colour primaries/transfer/matrix).
- [ ] The resolution / 10-bit caps **rejection** branches (only the AV1-support
      gate is hardware-proven; `WIDTH_MAX`/`HEIGHT_MAX`/`SUPPORT_10BIT_ENCODE`
      rejections aren't exercised on the 3090).

### AMF (AMD, RDNA3+)
- `CreateComponent(AMFVideoEncoderVCN_AV1)` self-validates ("RDNA3+ GPU
  required"); the rest is verified-by-review:
- [ ] End-to-end AV1 encode on RDNA3+ (RX 7000+): correct pixels, valid bitstream.
- [ ] 10-bit P010 encode (`Av1ColorBitDepth = 2`, P010 surface).
- [ ] Confirm `SubmitInput` back-pressure / surface-release path under sustained
      throughput (the encoder's in-flight tracking is verified-by-review).

### QSV (Intel, Arc / Meteor Lake+)
- `MFXVideoENCODE_Query` self-validates; the rest is verified-by-review:
- [ ] End-to-end AV1 encode on Arc / Meteor Lake+: correct pixels, valid bitstream.
- [ ] 10-bit P010 encode (`BitDepthLuma/Chroma = 10`, `Shift = 1`, P010 FourCC).
- [ ] ICQ vs CQP rate-control output quality on real silicon.

### Cross-vendor
- [ ] Multi-GPU single-file chunk stitching across **mixed vendors** (the
      cross-vendor `av1C` codec invariant is verified-by-review — confirm an
      NVENC + AMF/QSV mix on one rendition decodes cleanly).
- [ ] Optional: explicit AMF `GetCaps` / QSV implementation-caps query for full
      parity with NVENC's `GetEncodeGUIDs` enumeration (currently AMF/QSV rely on
      construction-time self-validation, which is sufficient but coarser).
