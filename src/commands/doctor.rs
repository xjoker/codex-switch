use std::path::Path;

use anyhow::Result;

use crate::codex_compat::{self, DoctorReport, VersionReport};
use crate::output::{OutputAlreadyReported, print_json};

pub(crate) fn doctor_cmd(desktop_codex: Option<&Path>, json: bool) -> Result<()> {
    let report = codex_compat::doctor_report(desktop_codex);
    if json {
        print_json(&report);
    } else {
        print_human_report(&report);
    }
    if report.ok {
        Ok(())
    } else {
        Err(OutputAlreadyReported.into())
    }
}

fn print_human_report(report: &DoctorReport) {
    println!("Codex minimum version: {}", report.minimum_version);
    println!("Currently aligned version: {}", report.aligned_version);
    println!("Note: {}", report.runtime_note);
    print_report("PATH CLI", &report.path_cli);
    print_report("Desktop engine", &report.desktop_codex);
    if let Some(matches) = report.versions_match {
        println!("  CLI and desktop engine versions match: {matches}");
    }
    if let Some(relation) = &report.version_relation {
        println!("  version relation: {relation}");
    }
}

fn print_report(label: &str, report: &VersionReport) {
    println!("{label}:");
    println!(
        "  executable: {}",
        report.executable.as_deref().unwrap_or("not found")
    );
    println!(
        "  version: {}",
        report.version.as_deref().unwrap_or("unknown")
    );
    println!("  status: {}", status_name(report.status));
    if let Some(note) = &report.note {
        println!("  note: {note}");
    }
}

fn status_name(status: crate::codex_compat::CompatibilityStatus) -> &'static str {
    use crate::codex_compat::CompatibilityStatus as Status;
    match status {
        Status::NotChecked => "not_checked",
        Status::NotFound => "not_found",
        Status::Unknown => "unknown",
        Status::DevBuild => "dev_build",
        Status::BelowMinimum => "below_minimum",
        Status::Aligned => "aligned",
        Status::AboveBaselineUnverified => "above_baseline_unverified",
    }
}
