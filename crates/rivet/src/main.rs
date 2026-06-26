//! `rivet` — command-line video transcoder.
//!
//! ```text
//! rivet transcode input.mkv -o output.mp4
//! rivet probe input.mkv
//! rivet probe input.mkv --json
//! ```
//!
//! Logging verbosity is controlled by `RUST_LOG` (e.g. `RUST_LOG=debug`).

use std::path::PathBuf;
use std::process::ExitCode;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use tracing_subscriber::EnvFilter;

#[derive(Parser)]
#[command(
    name = "rivet",
    version,
    about = "Modular GPU-accelerated video transcoder (AV1 + Opus / MP4).",
    long_about = None
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Transcode an input file to an AV1 + audio MP4.
    Transcode {
        /// Input media file (any supported container/codec).
        input: PathBuf,
        /// Output MP4 path. Defaults to `<input>.av1.mp4`.
        #[arg(short, long)]
        output: Option<PathBuf>,
    },
    /// Inspect an input file without transcoding it.
    Probe {
        /// Input media file.
        input: PathBuf,
        /// Emit machine-readable JSON instead of a human summary.
        #[arg(long)]
        json: bool,
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
        Command::Transcode { input, output } => {
            let output = output.unwrap_or_else(|| default_output_path(&input));
            let outcome = rivet::transcode_file(&input, &output)
                .with_context(|| format!("transcoding {}", input.display()))?;
            println!(
                "{} ({}x{} @ {:.3} fps) → {}",
                input.display(),
                outcome.input_dims.0,
                outcome.input_dims.1,
                outcome.input_frame_rate,
                output.display(),
            );
            println!(
                "  video: {} → av1 | {} frames, {} packets",
                outcome.input_codec, outcome.frames_processed, outcome.packets_emitted,
            );
            println!("  audio: {}", outcome.audio_handling.label());
            println!(
                "  {} bytes in → {} bytes out in {:.2}s",
                outcome.input_bytes,
                outcome.output_bytes.len(),
                outcome.elapsed.as_secs_f64(),
            );
            Ok(())
        }
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
    }
}

fn default_output_path(input: &std::path::Path) -> PathBuf {
    let stem = input
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| "output".to_string());
    let mut out = input.to_path_buf();
    out.set_file_name(format!("{stem}.av1.mp4"));
    out
}

fn print_probe(input: &std::path::Path, info: &rivet::MediaInfo) {
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
        Some(a) => println!(
            "  audio     : {} {} Hz {} ch",
            a.codec, a.sample_rate, a.channels
        ),
        None => println!("  audio     : (none)"),
    }
}

/// Hand-rolled JSON so the CLI stays dependency-light (no serde).
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
