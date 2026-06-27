//! `rivet` — command-line video transcoder.
//!
//! ```text
//! # Single MP4 (source resolution)
//! rivet transcode input.mkv -o output.mp4
//!
//! # Multi-rung ABR ladder of MP4s into a directory
//! rivet transcode input.mkv -o out_dir/ --rung 1920x1080 --rung 1280x720 --rung 640x360
//!
//! # Standard ladder, auto-derived from the source
//! rivet transcode input.mkv -o out_dir/ --ladder
//!
//! # CMAF/HLS package with 4-second segments
//! rivet transcode input.mkv -o hls_dir/ --mode hls --ladder --segment-seconds 4
//!
//! # Quality / audio knobs
//! rivet transcode input.mkv -o out.mp4 --crf 28 --speed 6 --audio opus
//!
//! rivet probe input.mkv [--json]
//! ```
//!
//! Logging verbosity is controlled by `RUST_LOG` (e.g. `RUST_LOG=debug`).

use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::Arc;

use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand, ValueEnum};
use tracing_subscriber::EnvFilter;

use rivet::progress::{RungProgress, RungStatus};
use rivet::spec::{
    AudioPolicy, BitDepth, ChunkSeamMode, ColorPolicy, EncodePolicy, GpuFamily, OutputSpec,
    Quality, Rung,
};
use rivet::{JobOutput, RungArtifact};

#[derive(Parser)]
#[command(
    name = "rivet",
    version,
    about = "Modular GPU-accelerated video transcoder (AV1 + Opus).",
    long_about = None
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Clone, Copy, ValueEnum)]
enum ModeArg {
    /// One self-contained MP4 per rung.
    Single,
    /// Segmented CMAF + HLS package.
    Hls,
}

#[derive(Clone, Copy, ValueEnum)]
enum AudioArg {
    /// Passthrough when possible, else transcode to Opus, else drop.
    Auto,
    /// Produce Opus audio.
    Opus,
    /// Drop audio (video only).
    Drop,
}

impl From<AudioArg> for AudioPolicy {
    fn from(a: AudioArg) -> Self {
        match a {
            AudioArg::Auto => AudioPolicy::Auto,
            AudioArg::Opus => AudioPolicy::ForceOpus,
            AudioArg::Drop => AudioPolicy::Drop,
        }
    }
}

#[derive(Clone, Copy, ValueEnum)]
enum GpuFamilyArg {
    Nvidia,
    Amd,
    Intel,
}

impl From<GpuFamilyArg> for GpuFamily {
    fn from(a: GpuFamilyArg) -> Self {
        match a {
            GpuFamilyArg::Nvidia => GpuFamily::Nvidia,
            GpuFamilyArg::Amd => GpuFamily::Amd,
            GpuFamilyArg::Intel => GpuFamily::Intel,
        }
    }
}

#[derive(Clone, Copy, ValueEnum)]
enum ColorArg {
    /// Tonemap HDR sources to SDR BT.709 (default).
    Sdr,
    /// HDR10: BT.2020 + PQ, 10-bit (needs a 10-bit encoder: nvidia/amd/qsv/ffmpeg).
    Hdr10,
    /// HLG: BT.2020 + ARIB STD-B67, 10-bit (needs a 10-bit encoder: nvidia/amd/qsv/ffmpeg).
    Hlg,
    /// Preserve the source color/transfer/bit-depth verbatim.
    Passthrough,
}

impl From<ColorArg> for ColorPolicy {
    fn from(a: ColorArg) -> Self {
        match a {
            ColorArg::Sdr => ColorPolicy::TonemapToSdr,
            ColorArg::Hdr10 => ColorPolicy::Hdr10,
            ColorArg::Hlg => ColorPolicy::Hlg,
            ColorArg::Passthrough => ColorPolicy::Passthrough,
        }
    }
}

#[derive(Clone, Copy, ValueEnum)]
enum PixelArg {
    /// Follow the color policy (default).
    Auto,
    #[value(name = "8bit")]
    Eight,
    #[value(name = "10bit")]
    Ten,
}

impl From<PixelArg> for BitDepth {
    fn from(a: PixelArg) -> Self {
        match a {
            PixelArg::Auto => BitDepth::Auto,
            PixelArg::Eight => BitDepth::EightBit,
            PixelArg::Ten => BitDepth::TenBit,
        }
    }
}

#[derive(Clone, Copy, ValueEnum)]
enum SeamArg {
    /// Chunk a single file across all GPUs for speed (default). NVENC chunks run
    /// VBR — possible mild quality steps at the ~2 s seams.
    Parallel,
    /// Chunk across GPUs but force constant-QP so seams are quality-flat. The QP
    /// is derived from the quality target, so quality still tracks it.
    Constqp,
    /// One encoder for the whole file: seam-free + quality-target-accurate, no
    /// multi-GPU single-file speedup.
    Serial,
}

impl From<SeamArg> for ChunkSeamMode {
    fn from(a: SeamArg) -> Self {
        match a {
            SeamArg::Parallel => ChunkSeamMode::Parallel,
            SeamArg::Constqp => ChunkSeamMode::ParallelConstQp,
            SeamArg::Serial => ChunkSeamMode::Serial,
        }
    }
}

#[derive(Subcommand)]
enum Command {
    /// Transcode an input file to AV1.
    Transcode {
        /// Input media file (any supported container/codec).
        input: PathBuf,
        /// Output path: a file (single mode, one rung) or a directory
        /// (single mode multi-rung, or HLS). Defaults to `<input>.av1.mp4`
        /// for the simple single-rung case.
        #[arg(short, long)]
        output: Option<PathBuf>,
        /// Output mode.
        #[arg(long, value_enum, default_value = "single")]
        mode: ModeArg,
        /// A ladder rung as `WxH` (repeatable). If omitted, a single rung at
        /// the source resolution is used (unless `--ladder` is set).
        #[arg(long = "rung", value_name = "WxH")]
        rungs: Vec<String>,
        /// Auto-derive a standard ABR ladder from the source resolution.
        #[arg(long)]
        ladder: bool,
        /// Ladder cap on the short side (with `--ladder`). Default 1080.
        #[arg(long)]
        max_short_side: Option<u32>,
        /// Target segment length in seconds (HLS mode).
        #[arg(long, default_value_t = 4.0)]
        segment_seconds: f32,
        /// Constant rate factor (encoder-native, lower = better quality).
        #[arg(long)]
        crf: Option<u8>,
        /// Encoder speed preset (encoder-native).
        #[arg(long)]
        speed: Option<u8>,
        /// Audio handling.
        #[arg(long, value_enum, default_value = "auto")]
        audio: AudioArg,
        /// Cap the output frame rate.
        #[arg(long)]
        max_fps: Option<f64>,
        /// Pin hardware encode/decode to this GPU index (implies single-GPU).
        #[arg(long)]
        gpu: Option<u32>,
        /// Encode serially on a single GPU instead of chunk-encoding across all
        /// GPUs. Without `--gpu N` this picks the first GPU. Default: all GPUs.
        #[arg(long)]
        single_gpu: bool,
        /// Constrain encode to one GPU vendor family (e.g. all NVIDIA cards,
        /// ignoring an integrated AMD/Intel GPU).
        #[arg(long, value_enum)]
        gpu_family: Option<GpuFamilyArg>,
        /// Pin the decode pump to this GPU index (default: follows the encode
        /// policy). E.g. decode on an iGPU while the dGPUs encode.
        #[arg(long)]
        decode_gpu: Option<u32>,
        /// Output color / tonemap policy.
        #[arg(long, value_enum, default_value = "sdr")]
        color: ColorArg,
        /// Output luma bit depth.
        #[arg(long, value_enum, default_value = "auto")]
        pixel_format: PixelArg,
        /// Multi-GPU single-file chunk seam handling: `parallel` (fastest),
        /// `constqp` (seam-flat constant-QP, quality still tracks the target), or
        /// `serial` (one encoder, seam-free, no multi-GPU single-file speedup).
        #[arg(long = "seam-mode", value_enum, default_value = "parallel")]
        seam_mode: SeamArg,
    },
    /// Inspect an input file without transcoding it.
    Probe {
        /// Input media file.
        input: PathBuf,
        /// Emit machine-readable JSON instead of a human summary.
        #[arg(long)]
        json: bool,
    },
    /// List detected GPU devices (vendor, name, VRAM, AV1-encode, live load).
    Devices {
        /// Emit machine-readable JSON instead of a human table.
        #[arg(long)]
        json: bool,
    },
    /// Report what this build + host can do: enabled backends, encode/decode
    /// codec support, and the detected devices.
    #[command(visible_alias = "caps")]
    Capabilities {
        /// Emit machine-readable JSON instead of a human summary.
        #[arg(long)]
        json: bool,
    },
    /// Stream a transcode: read media from **stdin**, write the AV1/MP4 to
    /// **stdout**. With no options it's the source-resolution single-file
    /// default; the flags override quality/size/color/audio. E.g.
    /// `cat in.mkv | rivet pipe --crf 28 --color hdr10 > out.mp4`.
    Pipe {
        /// Constant rate factor (lower = higher quality).
        #[arg(long)]
        crf: Option<u8>,
        /// Encoder speed preset.
        #[arg(long)]
        speed: Option<u8>,
        /// Audio policy.
        #[arg(long, value_enum)]
        audio: Option<AudioArg>,
        /// Output color / tonemap policy.
        #[arg(long, value_enum)]
        color: Option<ColorArg>,
        /// Output bit depth.
        #[arg(long = "bit-depth", visible_alias = "pixel-format", value_enum)]
        bit_depth: Option<PixelArg>,
        /// Cap the output frame rate.
        #[arg(long = "max-fps")]
        max_fps: Option<f64>,
        /// Output width (scales; defaults to source).
        #[arg(long)]
        width: Option<u32>,
        /// Output height (scales; defaults to source).
        #[arg(long)]
        height: Option<u32>,
        /// Pin encode to this GPU index.
        #[arg(long)]
        gpu: Option<u32>,
    },
    /// Run a **Unix-domain-socket** IPC server (Unix only). Each connection: the
    /// client writes media, half-closes its write side, then reads the
    /// transcoded AV1/MP4 back. Per-job settings can prefix the stream as a
    /// `#rivet key=value …\n` header line. Lets an app stream data in and out
    /// without HTTP or temp files.
    Ipc {
        /// Socket path to bind, e.g. `/tmp/rivet.sock`.
        #[arg(long)]
        socket: PathBuf,
    },
    /// Run the HTTP transcode API server so another app can signal transcodes
    /// over the network (needs the `server` feature).
    #[cfg(feature = "server")]
    Serve {
        /// Address to bind, e.g. `0.0.0.0:8080`.
        #[arg(long, default_value = "127.0.0.1:8080")]
        addr: String,
    },
}

fn main() -> ExitCode {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")))
        .with_writer(std::io::stderr)
        .init();

    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("error: {e:#}");
            ExitCode::FAILURE
        }
    }
}

fn run() -> Result<()> {
    let cli = Cli::parse();
    match cli.command {
        Command::Transcode {
            input,
            output,
            mode,
            rungs,
            ladder,
            max_short_side,
            segment_seconds,
            crf,
            speed,
            audio,
            max_fps,
            gpu,
            single_gpu,
            gpu_family,
            decode_gpu,
            color,
            pixel_format,
            seam_mode,
        } => transcode_cmd(TranscodeArgs {
            input,
            output,
            mode,
            rungs,
            ladder,
            max_short_side,
            segment_seconds,
            crf,
            speed,
            audio,
            max_fps,
            gpu,
            single_gpu,
            gpu_family,
            decode_gpu,
            color,
            pixel_format,
            seam_mode,
        }),
        Command::Probe { input, json } => {
            let info = rivet::probe_file(&input)
                .with_context(|| format!("probing {}", input.display()))?;
            if json {
                println!("{}", probe_json(&info));
            } else {
                print_probe(&input, &info);
            }
            Ok(())
        }
        Command::Devices { json } => {
            devices_cmd(json);
            Ok(())
        }
        Command::Capabilities { json } => {
            capabilities_cmd(json);
            Ok(())
        }
        Command::Pipe {
            crf,
            speed,
            audio,
            color,
            bit_depth,
            max_fps,
            width,
            height,
            gpu,
        } => pipe_cmd(JobSettings {
            crf,
            speed,
            audio: audio.map(Into::into),
            color: color.map(Into::into),
            bit_depth: bit_depth.map(Into::into),
            max_fps,
            width,
            height,
            gpu,
        }),
        Command::Ipc { socket } => ipc_cmd(&socket),
        #[cfg(feature = "server")]
        Command::Serve { addr } => {
            let addr: std::net::SocketAddr = addr.parse().context("parsing --addr")?;
            let rt = tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .build()
                .context("building tokio runtime")?;
            eprintln!("rivet transcode API on http://{addr} (POST media to /v1/transcode)");
            rt.block_on(rivet::server::serve(addr))
        }
    }
}

struct TranscodeArgs {
    input: PathBuf,
    output: Option<PathBuf>,
    mode: ModeArg,
    rungs: Vec<String>,
    ladder: bool,
    max_short_side: Option<u32>,
    segment_seconds: f32,
    crf: Option<u8>,
    speed: Option<u8>,
    audio: AudioArg,
    max_fps: Option<f64>,
    gpu: Option<u32>,
    single_gpu: bool,
    gpu_family: Option<GpuFamilyArg>,
    decode_gpu: Option<u32>,
    color: ColorArg,
    pixel_format: PixelArg,
    seam_mode: SeamArg,
}

fn transcode_cmd(args: TranscodeArgs) -> Result<()> {
    let bytes = std::fs::read(&args.input)
        .with_context(|| format!("reading input {}", args.input.display()))?;

    // Probe to resolve the ladder when not given explicitly.
    let probed = rivet::probe_bytes(&bytes).context("probing input")?;

    let quality = Quality {
        crf: args.crf,
        speed_preset: args.speed,
        ..Default::default()
    };

    let rungs = resolve_rungs(&args, &probed, &quality)?;
    if rungs.is_empty() {
        bail!("no rungs to produce (check --rung / --ladder and the source resolution)");
    }

    let audio = match args.audio {
        AudioArg::Auto => AudioPolicy::Auto,
        AudioArg::Opus => AudioPolicy::ForceOpus,
        AudioArg::Drop => AudioPolicy::Drop,
    };

    let mut spec = match args.mode {
        ModeArg::Single => OutputSpec::single_file(rungs),
        ModeArg::Hls => OutputSpec::hls(rungs, args.segment_seconds),
    };
    spec.audio = audio;
    spec.max_frame_rate = args.max_fps;
    spec = if let Some(idx) = args.gpu {
        spec.encode_policy(EncodePolicy::SingleGpu(Some(idx)))
    } else if let Some(fam) = args.gpu_family {
        spec.encode_policy(EncodePolicy::Family(fam.into()))
    } else if args.single_gpu {
        spec.encode_policy(EncodePolicy::SingleGpu(None))
    } else {
        spec.encode_policy(EncodePolicy::AllGpus)
    };
    spec = spec.decode_gpu(args.decode_gpu);
    spec = spec
        .with_color(args.color.into())
        .with_bit_depth(args.pixel_format.into())
        .chunk_seam_mode(args.seam_mode.into());

    // Progress: one carriage-return line per rung update.
    let sink = Arc::new(rivet::fn_sink(|p: RungProgress| {
        eprintln!(
            "  [{:>6}] {:<6} {:>5.1}%  {} frames{}",
            p.label,
            status_str(p.status),
            p.percent,
            p.frames_done,
            p.message.as_deref().map(|m| format!("  ({m})")).unwrap_or_default(),
        );
    }));

    // Determine output target.
    let (output_dir, single_file_target) = plan_output(&args)?;

    let out = rivet::run_job_blocking(
        &bytes,
        &spec,
        output_dir.as_deref(),
        sink,
    )
    .with_context(|| format!("transcoding {}", args.input.display()))?;

    write_outputs(&args, &out, output_dir.as_deref(), single_file_target.as_deref())?;
    print_summary(&args.input, &out);
    Ok(())
}

/// Build the rung list from `--rung` / `--ladder` / default-source.
fn resolve_rungs(args: &TranscodeArgs, probed: &rivet::MediaInfo, quality: &Quality) -> Result<Vec<Rung>> {
    if !args.rungs.is_empty() {
        let mut out = Vec::new();
        for s in &args.rungs {
            let (w, h) = parse_wxh(s)?;
            out.push(Rung::new(w, h).with_quality(quality.clone()));
        }
        return Ok(out);
    }
    if args.ladder {
        return Ok(rivet::ladder::standard_ladder_with_quality(
            probed.width,
            probed.height,
            args.max_short_side,
            quality.clone(),
        ));
    }
    // Default: single rung at the source resolution.
    let (w, h) = (probed.width & !1, probed.height & !1);
    if w == 0 || h == 0 {
        bail!("source resolution unknown ({}x{}); specify --rung", probed.width, probed.height);
    }
    Ok(vec![Rung::new(w, h).with_quality(quality.clone())])
}

/// Decide where outputs go. Returns (hls/multi output dir, single-file target).
fn plan_output(args: &TranscodeArgs) -> Result<(Option<PathBuf>, Option<PathBuf>)> {
    match args.mode {
        ModeArg::Hls => {
            let dir = args
                .output
                .clone()
                .unwrap_or_else(|| default_dir(&args.input, "hls"));
            std::fs::create_dir_all(&dir)
                .with_context(|| format!("creating output dir {}", dir.display()))?;
            Ok((Some(dir), None))
        }
        ModeArg::Single => {
            // Multi-rung → directory; single-rung → file.
            let multi = args.rungs.len() > 1 || args.ladder;
            if multi {
                let dir = args
                    .output
                    .clone()
                    .unwrap_or_else(|| default_dir(&args.input, "av1"));
                std::fs::create_dir_all(&dir)
                    .with_context(|| format!("creating output dir {}", dir.display()))?;
                // SingleFile bytes are returned in memory; write_outputs places
                // each rung at `<dir>/<label>.mp4`.
                Ok((Some(dir), None))
            } else {
                let file = args
                    .output
                    .clone()
                    .unwrap_or_else(|| default_file(&args.input));
                Ok((None, Some(file)))
            }
        }
    }
}

fn write_outputs(
    args: &TranscodeArgs,
    out: &JobOutput,
    output_dir: Option<&Path>,
    single_file_target: Option<&Path>,
) -> Result<()> {
    match args.mode {
        ModeArg::Hls => {
            // HLS package already written under output_dir by the engine.
        }
        ModeArg::Single => {
            if let Some(file) = single_file_target {
                // Exactly one rung.
                if let Some(r) = out.rungs.first() {
                    if let RungArtifact::File(bytes) = &r.artifact {
                        std::fs::write(file, bytes)
                            .with_context(|| format!("writing {}", file.display()))?;
                    }
                }
            } else if let Some(dir) = output_dir {
                for r in &out.rungs {
                    if let RungArtifact::File(bytes) = &r.artifact {
                        let path = dir.join(format!("{}.mp4", r.label));
                        std::fs::write(&path, bytes)
                            .with_context(|| format!("writing {}", path.display()))?;
                    }
                }
            }
        }
    }
    Ok(())
}

fn print_summary(input: &Path, out: &JobOutput) {
    println!(
        "{} ({}x{} @ {:.3} fps {})",
        input.display(),
        out.source_dims.0,
        out.source_dims.1,
        out.source_frame_rate,
        out.source_codec,
    );
    println!("  audio: {}", out.audio_handling);
    for r in &out.rungs {
        let where_ = match &r.artifact {
            RungArtifact::File(_) => "mp4".to_string(),
            RungArtifact::HlsRendition { relative_dir, .. } => relative_dir.clone(),
        };
        println!(
            "  {:<6} {}x{}  {} frames  {:.2} MiB  [{}]",
            r.label,
            r.width,
            r.height,
            r.frames,
            r.bytes as f64 / (1024.0 * 1024.0),
            where_,
        );
    }
    if let Some(master) = &out.master_playlist {
        println!("  master playlist: {}", master.display());
    }
    println!("  done in {:.2}s", out.elapsed.as_secs_f64());
}

fn parse_wxh(s: &str) -> Result<(u32, u32)> {
    let (w, h) = s
        .split_once(['x', 'X'])
        .ok_or_else(|| anyhow::anyhow!("rung '{s}' is not WxH (e.g. 1280x720)"))?;
    let w: u32 = w.trim().parse().with_context(|| format!("bad width in '{s}'"))?;
    let h: u32 = h.trim().parse().with_context(|| format!("bad height in '{s}'"))?;
    if w == 0 || h == 0 {
        bail!("rung '{s}' has a zero dimension");
    }
    Ok((w & !1, h & !1))
}

fn default_file(input: &Path) -> PathBuf {
    let stem = input
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| "output".to_string());
    let mut out = input.to_path_buf();
    out.set_file_name(format!("{stem}.av1.mp4"));
    out
}

fn default_dir(input: &Path, suffix: &str) -> PathBuf {
    let stem = input
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| "output".to_string());
    let mut out = input.to_path_buf();
    out.set_file_name(format!("{stem}.{suffix}"));
    out
}

fn status_str(s: RungStatus) -> &'static str {
    match s {
        RungStatus::Pending => "pend",
        RungStatus::Running => "run",
        RungStatus::Finalizing => "final",
        RungStatus::Completed => "done",
        RungStatus::Failed => "FAIL",
    }
}

fn print_probe(input: &Path, info: &rivet::MediaInfo) {
    println!("{}", input.display());
    println!("  container : {}", info.container);
    println!("  video     : {}", info.video_codec);
    println!("  dimensions: {}x{}", info.width, info.height);
    println!("  frame rate: {:.3} fps", info.frame_rate);
    if info.duration > 0.0 {
        println!("  duration  : {:.3} s", info.duration);
    }
    println!("  pixel fmt : {}", info.pixel_format);
    match &info.audio {
        Some(a) => println!("  audio     : {} {} Hz {} ch", a.codec, a.sample_rate, a.channels),
        None => println!("  audio     : (none)"),
    }
}

fn probe_json(info: &rivet::MediaInfo) -> String {
    let audio = match &info.audio {
        Some(a) => format!(
            "{{\"codec\":\"{}\",\"sample_rate\":{},\"channels\":{}}}",
            esc(&a.codec),
            a.sample_rate,
            a.channels
        ),
        None => "null".to_string(),
    };
    format!(
        "{{\"container\":\"{}\",\"video_codec\":\"{}\",\"width\":{},\"height\":{},\"frame_rate\":{},\"duration\":{},\"pixel_format\":\"{}\",\"audio\":{}}}",
        esc(&info.container),
        esc(&info.video_codec),
        info.width,
        info.height,
        info.frame_rate,
        info.duration,
        esc(&info.pixel_format),
        audio,
    )
}

fn esc(s: &str) -> String {
    s.replace('\\', "\\\\").replace('"', "\\\"")
}

// ── `rivet devices` ────────────────────────────────────────────────

fn devices_cmd(json: bool) {
    let devices = codec::gpu::detect_gpus();
    if json {
        println!("{}", devices_json(&devices));
        return;
    }
    if devices.is_empty() {
        println!(
            "No GPUs detected (CPU-only host). GPU transcode needs a `nvidia` / `amd` / `qsv` \
             feature build with the matching hardware; the `ffmpeg` feature provides software."
        );
        return;
    }
    let util = codec::gpu::GpuUtilizationReader::new();
    println!("{} GPU(s) detected:\n", devices.len());
    for d in &devices {
        println!(
            "  [{}] {} {}",
            d.index,
            codec::gpu::manufacturer_label(d.vendor),
            d.name
        );
        println!("      generation : {}", d.generation);
        if d.vram_mib > 0 {
            println!("      VRAM       : {} MiB", d.vram_mib);
        }
        println!("      PCI        : {}", d.host_pci_address);
        // Live load is read via NVML — meaningful on NVIDIA only.
        if matches!(d.vendor, codec::gpu::GpuVendor::Nvidia) {
            let u = util.read(d);
            print!(
                "      load       : gpu {}% · enc {}% · dec {}% · mem {}/{} MiB",
                u.util_percent, u.encoder_percent, u.decoder_percent, u.mem_used_mib, u.mem_total_mib
            );
            if let Some(t) = u.temperature_c {
                print!(" · {t}°C");
            }
            println!();
        }
        println!();
    }
    println!("Run `rivet capabilities` for what this build can encode/decode.");
}

fn devices_json(devices: &[codec::gpu::GpuDevice]) -> String {
    let util = codec::gpu::GpuUtilizationReader::new();
    let items: Vec<String> = devices
        .iter()
        .map(|d| {
            let load = if matches!(d.vendor, codec::gpu::GpuVendor::Nvidia) {
                let u = util.read(d);
                let temp = u
                    .temperature_c
                    .map(|t| t.to_string())
                    .unwrap_or_else(|| "null".into());
                format!(
                    ",\"load\":{{\"gpu_percent\":{},\"encoder_percent\":{},\"decoder_percent\":{},\"mem_used_mib\":{},\"mem_total_mib\":{},\"temperature_c\":{}}}",
                    u.util_percent, u.encoder_percent, u.decoder_percent, u.mem_used_mib, u.mem_total_mib, temp
                )
            } else {
                String::new()
            };
            format!(
                "{{\"index\":{},\"vendor\":\"{}\",\"name\":\"{}\",\"generation\":\"{}\",\"vram_mib\":{},\"pci\":\"{}\"{}}}",
                d.index,
                codec::gpu::manufacturer_label(d.vendor),
                esc(&d.name),
                esc(&d.generation),
                d.vram_mib,
                esc(&d.host_pci_address),
                load
            )
        })
        .collect();
    format!("{{\"gpus\":[{}]}}", items.join(","))
}

// ── `rivet capabilities` ───────────────────────────────────────────

fn capabilities_cmd(json: bool) {
    let enc = codec::encode::encode_backends();
    let dec_backends = codec::decode::decode_backends();
    let caps = codec::encode::build_output_caps();
    let dec = codec::decode::decode_capabilities();
    let devices = codec::gpu::detect_gpus();

    if json {
        let enc_b = enc
            .iter()
            .map(|b| format!("\"{b}\""))
            .collect::<Vec<_>>()
            .join(",");
        let dec_b = dec_backends
            .iter()
            .map(|b| format!("\"{b}\""))
            .collect::<Vec<_>>()
            .join(",");
        let codecs = dec
            .iter()
            .map(|d| {
                let bs = d
                    .backends
                    .iter()
                    .map(|b| format!("\"{b}\""))
                    .collect::<Vec<_>>()
                    .join(",");
                format!("{{\"codec\":\"{}\",\"backends\":[{}]}}", d.codec, bs)
            })
            .collect::<Vec<_>>()
            .join(",");
        println!(
            "{{\"encode\":{{\"codec\":\"av1\",\"backends\":[{}],\"max_bit_depth\":{},\"hdr\":{}}},\
             \"decode\":{{\"backends\":[{}],\"codecs\":[{}]}},\"devices\":{}}}",
            enc_b,
            caps.max_bit_depth,
            caps.hdr,
            dec_b,
            codecs,
            devices_json(&devices)
        );
        return;
    }

    println!("rivet capabilities\n");
    println!("Encode — AV1 (4:2:0):");
    if enc.is_empty() {
        println!("  (none) build with a `nvidia` / `amd` / `qsv` / `ffmpeg` feature");
    } else {
        println!("  backends   : {}", enc.join(", "));
        println!("  max depth  : {}-bit", caps.max_bit_depth);
        println!(
            "  HDR        : {}",
            if caps.hdr {
                "yes (PQ / HLG, BT.2020, 10-bit)"
            } else {
                "no"
            }
        );
    }

    println!("\nDecode — codec → backends:");
    if dec_backends.is_empty() {
        println!("  (none) build with a `nvidia` / `amd` / `qsv` / `ffmpeg` feature");
    } else {
        for d in &dec {
            let b = if d.backends.is_empty() {
                "—".to_string()
            } else {
                d.backends.join(", ")
            };
            println!("  {:<8} {}", d.codec, b);
        }
    }

    println!("\nDevices — {} detected:", devices.len());
    if devices.is_empty() {
        println!("  (none) CPU-only host — only the `ffmpeg` software path can run here");
    } else {
        for dv in &devices {
            print!(
                "  [{}] {} {}",
                dv.index,
                codec::gpu::manufacturer_label(dv.vendor),
                dv.name
            );
            if dv.vram_mib > 0 {
                print!(" ({} MiB)", dv.vram_mib);
            }
            println!();
        }
    }
}

// ── streaming transcode settings (shared by `pipe` flags / `ipc` header) ──

/// Per-job settings for the streaming paths. All-`None` keeps the single-file
/// default (fast `transcode_bytes` path); any set field routes through the full
/// `run_job` single-file engine (scale / color / bit-depth / quality / audio /
/// gpu). `pipe` fills this from CLI flags; `ipc` from a `#rivet k=v …` header.
#[derive(Default, Clone)]
struct JobSettings {
    crf: Option<u8>,
    speed: Option<u8>,
    audio: Option<AudioPolicy>,
    color: Option<ColorPolicy>,
    bit_depth: Option<BitDepth>,
    max_fps: Option<f64>,
    width: Option<u32>,
    height: Option<u32>,
    gpu: Option<u32>,
}

impl JobSettings {
    fn is_empty(&self) -> bool {
        self.crf.is_none()
            && self.speed.is_none()
            && self.audio.is_none()
            && self.color.is_none()
            && self.bit_depth.is_none()
            && self.max_fps.is_none()
            && self.width.is_none()
            && self.height.is_none()
            && self.gpu.is_none()
    }

    /// Parse a `key=value key=value …` line (the `rivet ipc` header body).
    fn parse_kv(line: &str) -> Result<Self> {
        let mut s = Self::default();
        for tok in line.split_whitespace() {
            let (k, v) = tok
                .split_once('=')
                .ok_or_else(|| anyhow::anyhow!("bad setting '{tok}' (expected key=value)"))?;
            match k {
                "crf" => s.crf = Some(v.parse().context("crf")?),
                "speed" => s.speed = Some(v.parse().context("speed")?),
                "audio" => {
                    s.audio = Some(match v {
                        "auto" => AudioPolicy::Auto,
                        "opus" => AudioPolicy::ForceOpus,
                        "drop" => AudioPolicy::Drop,
                        o => bail!("audio must be auto|opus|drop, got '{o}'"),
                    })
                }
                "color" => {
                    s.color = Some(match v {
                        "sdr" => ColorPolicy::TonemapToSdr,
                        "hdr10" => ColorPolicy::Hdr10,
                        "hlg" => ColorPolicy::Hlg,
                        "passthrough" => ColorPolicy::Passthrough,
                        o => bail!("color must be sdr|hdr10|hlg|passthrough, got '{o}'"),
                    })
                }
                "bit-depth" | "pixel-format" => {
                    s.bit_depth = Some(match v {
                        "auto" => BitDepth::Auto,
                        "8bit" => BitDepth::EightBit,
                        "10bit" => BitDepth::TenBit,
                        o => bail!("bit-depth must be auto|8bit|10bit, got '{o}'"),
                    })
                }
                "max-fps" => s.max_fps = Some(v.parse().context("max-fps")?),
                "width" => s.width = Some(v.parse().context("width")?),
                "height" => s.height = Some(v.parse().context("height")?),
                "gpu" => s.gpu = Some(v.parse().context("gpu")?),
                o => bail!(
                    "unknown setting '{o}' (crf/speed/audio/color/bit-depth/max-fps/width/height/gpu)"
                ),
            }
        }
        Ok(s)
    }

    /// Build a single-file `OutputSpec` from these settings, run it, and return
    /// `(mp4_bytes, frames, audio_label)`.
    fn run(&self, input: &[u8]) -> Result<(Vec<u8>, u64, String)> {
        let probed = rivet::probe_bytes(input).context("probing input")?;
        let w = self.width.unwrap_or(probed.width);
        let h = self.height.unwrap_or(probed.height);
        if w == 0 || h == 0 {
            bail!("could not determine output size from the source — set width=/height=");
        }
        let quality = Quality {
            crf: self.crf,
            speed_preset: self.speed,
            ..Default::default()
        };
        let mut spec = OutputSpec::single_file(vec![Rung::new(w, h).with_quality(quality)]);
        if let Some(a) = self.audio {
            spec.audio = a;
        }
        spec.max_frame_rate = self.max_fps;
        if let Some(g) = self.gpu {
            spec = spec.encode_policy(EncodePolicy::SingleGpu(Some(g)));
        }
        if let Some(c) = self.color {
            spec = spec.with_color(c);
        }
        if let Some(b) = self.bit_depth {
            spec = spec.with_bit_depth(b);
        }
        spec.validate().context("invalid settings")?;
        let sink = Arc::new(rivet::fn_sink(|_p: RungProgress| {}));
        let out = rivet::run_job_blocking(input, &spec, None, sink).context("transcoding")?;
        let audio = out.audio_handling.clone();
        for r in out.rungs {
            let frames = r.frames;
            if let rivet::RungArtifact::File(bytes) = r.artifact {
                return Ok((bytes, frames, audio));
            }
        }
        bail!("no single-file output produced")
    }
}

/// Transcode `input` honoring `settings`: all-default settings take the fast
/// `transcode_bytes` path; any set field routes through the full `run_job`
/// single-file engine. Returns `(mp4_bytes, frames, audio_label)`.
fn stream_transcode(input: &[u8], settings: &JobSettings) -> Result<(Vec<u8>, u64, String)> {
    if settings.is_empty() {
        let out = rivet::transcode_bytes(input).context("transcoding")?;
        Ok((
            out.output_bytes,
            out.frames_processed,
            out.audio_handling.label(),
        ))
    } else {
        settings.run(input)
    }
}

// ── `rivet pipe` — stdin → stdout streaming ────────────────────────

fn pipe_cmd(settings: JobSettings) -> Result<()> {
    use std::io::{Read, Write};
    let mut input = Vec::new();
    std::io::stdin()
        .lock()
        .read_to_end(&mut input)
        .context("reading media from stdin")?;
    if input.is_empty() {
        bail!("empty stdin — pipe media in, e.g. `cat in.mkv | rivet pipe > out.mp4`");
    }
    eprintln!("rivet pipe: {} bytes in, transcoding…", input.len());
    let (bytes, frames, audio) = stream_transcode(&input, &settings)?;
    let mut stdout = std::io::stdout().lock();
    stdout.write_all(&bytes).context("writing AV1/MP4 to stdout")?;
    stdout.flush().ok();
    eprintln!("rivet pipe: {frames} frames → {} bytes out ({audio})", bytes.len());
    Ok(())
}

// ── `rivet ipc` — Unix-domain-socket streaming server ──────────────

/// Split an optional `#rivet key=value …\n` settings header off the front of
/// the stream. Real container magic bytes never start with `#rivet`, so this is
/// unambiguous. Returns the parsed settings and the remaining media slice.
#[cfg(unix)]
fn split_ipc_settings(input: &[u8]) -> (Result<JobSettings>, &[u8]) {
    const MAGIC: &[u8] = b"#rivet";
    if input.starts_with(MAGIC) {
        let nl = input.iter().position(|&b| b == b'\n').unwrap_or(input.len());
        let media_start = (nl + 1).min(input.len());
        let line = std::str::from_utf8(&input[MAGIC.len()..nl])
            .map(str::trim)
            .unwrap_or("");
        (JobSettings::parse_kv(line), &input[media_start..])
    } else {
        (Ok(JobSettings::default()), input)
    }
}

#[cfg(unix)]
fn ipc_cmd(socket: &Path) -> Result<()> {
    use std::io::{Read, Write};
    use std::os::unix::net::{UnixListener, UnixStream};

    // Drop a stale socket from a previous run (ignore "not found").
    let _ = std::fs::remove_file(socket);
    let listener = UnixListener::bind(socket)
        .with_context(|| format!("binding Unix socket {}", socket.display()))?;
    eprintln!(
        "rivet ipc: listening on {}\n           per connection: [optional `#rivet k=v …\\n` header] media → half-close → read AV1/MP4 back\n           e.g.  socat - UNIX-CONNECT:{} < in.mkv > out.mp4",
        socket.display(),
        socket.display(),
    );

    fn handle(mut stream: UnixStream) {
        let mut input = Vec::new();
        if let Err(e) = stream.read_to_end(&mut input) {
            eprintln!("rivet ipc: read error: {e}");
            return;
        }
        if input.is_empty() {
            return; // probe/keepalive connection
        }
        let (settings, media) = split_ipc_settings(&input);
        let settings = match settings {
            Ok(s) => s,
            Err(e) => {
                eprintln!("rivet ipc: bad settings header: {e:#}");
                return;
            }
        };
        eprintln!("rivet ipc: {} media bytes in", media.len());
        match stream_transcode(media, &settings) {
            Ok((bytes, frames, audio)) => {
                if let Err(e) = stream.write_all(&bytes) {
                    eprintln!("rivet ipc: write error: {e}");
                    return;
                }
                stream.flush().ok();
                let _ = stream.shutdown(std::net::Shutdown::Write);
                eprintln!("rivet ipc: {frames} frames → {} bytes out ({audio})", bytes.len());
            }
            Err(e) => eprintln!("rivet ipc: transcode error: {e:#}"),
        }
    }

    for stream in listener.incoming() {
        match stream {
            // One thread per connection; the process-wide GPU pool serializes
            // the actual GPU work, so concurrent clients just queue on it.
            Ok(s) => {
                std::thread::spawn(move || handle(s));
            }
            Err(e) => eprintln!("rivet ipc: accept error: {e}"),
        }
    }
    Ok(())
}

#[cfg(not(unix))]
fn ipc_cmd(_socket: &Path) -> Result<()> {
    bail!(
        "`rivet ipc` (Unix-domain socket) is Unix-only. On Windows, use \
         `rivet pipe` (stdin/stdout) or `rivet serve` (HTTP)."
    )
}
