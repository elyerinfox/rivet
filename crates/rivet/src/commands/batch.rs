//! Implementation of `rivet batch` (YAML/JSON manifest runner).

use std::path::Path;

use anyhow::{bail, Context, Result};
use rivet::manifest;

pub(crate) fn run(manifest_path: &Path, dry_run: bool, stop_on_error: bool) -> Result<()> {
    let text = std::fs::read_to_string(manifest_path)
        .with_context(|| format!("reading manifest {}", manifest_path.display()))?;
    let mut m = manifest::parse_manifest(&text, manifest::Format::from_path(manifest_path))?;
    if stop_on_error {
        m.on_error = Some("stop".into());
    }
    let base = manifest_path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."))
        .to_path_buf();

    if dry_run {
        let planned = manifest::plan_manifest(&m, &base)?;
        eprintln!("batch dry-run: {} job(s) planned\n", planned.len());
        for (i, job) in planned.iter().enumerate() {
            let s = &job.spec;
            let mode = s.mode.as_deref().unwrap_or("single");
            let mut bits = vec![format!("mode={mode}")];
            if s.ladder == Some(true) {
                bits.push("ladder".into());
            }
            if let Some(r) = &s.rungs {
                bits.push(format!("rungs={}", r.join(",")));
            }
            if let Some(c) = s.crf {
                bits.push(format!("crf={c}"));
            }
            if let Some(c) = &s.color {
                bits.push(format!("color={c}"));
            }
            if let Some(o) = &s.output {
                bits.push(format!("output={o}"));
            }
            eprintln!("  [{}] {}  ({})", i + 1, job.input.display(), bits.join(" "));
        }
        eprintln!("\n(dry run — nothing converted)");
        return Ok(());
    }

    let report = manifest::run_manifest(&m, &base)?;

    println!(
        "\nbatch: {} ok, {} failed (of {})",
        report.ok_count(),
        report.failed_count(),
        report.outcomes.len()
    );
    for o in &report.outcomes {
        match &o.status {
            manifest::JobStatus::Ok => println!(
                "  ok    {} -> {}",
                o.input.display(),
                o.output.as_ref().map(|p| p.display().to_string()).unwrap_or_default()
            ),
            manifest::JobStatus::Failed(e) => {
                println!("  FAIL  {}: {}", o.input.display(), e)
            }
        }
    }
    if !report.all_ok() {
        bail!("{} job(s) failed", report.failed_count());
    }
    Ok(())
}
