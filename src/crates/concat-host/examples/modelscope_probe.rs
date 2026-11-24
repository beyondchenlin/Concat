// SPDX-License-Identifier: AGPL-3.0-or-later
//! Explicit network acceptance probe, never part of an ordinary test run.
//! cargo run -p concat-host --example modelscope_probe -- <empty output folder>
//! Reads only the saved download-source choice. Downloads the real tiny model
//! with the production downloader; all files stay in the supplied test folder.
use concat_host::{dirs::AppDirs, models};
use serde_json::json;
use std::path::PathBuf;
use std::sync::{Mutex, atomic::AtomicBool};

struct Receipts(Mutex<Vec<String>>);
impl log::Log for Receipts {
    fn enabled(&self, metadata: &log::Metadata<'_>) -> bool {
        metadata.target().starts_with("concat_host::models")
    }
    fn log(&self, record: &log::Record<'_>) {
        if self.enabled(record.metadata()) {
            let message = record.args().to_string();
            if message.starts_with("models: verified ") {
                eprintln!("{message}");
                self.0.lock().expect("receipt log").push(message);
            }
        }
    }
    fn flush(&self) {}
}
static RECEIPTS: Receipts = Receipts(Mutex::new(Vec::new()));

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let output = std::env::args_os()
        .nth(1)
        .map(PathBuf::from)
        .ok_or("pass an empty test output directory; the user model folder is never replaced")?;
    if output.exists() && std::fs::read_dir(&output)?.next().is_some() {
        return Err("the test output folder is not empty; previous evidence is preserved".into());
    }
    std::fs::create_dir_all(&output)?;
    let dirs = AppDirs::locate()?;
    let settings = dirs.config.join("settings.json");
    let preferences: serde_json::Value = if settings.is_file() {
        serde_json::from_slice(&std::fs::read(&settings)?)?
    } else {
        json!({})
    };
    let saved = preferences
        .get("downloadSource")
        .and_then(|value| value.as_str());
    let preference = models::SourcePreference::parse(saved.unwrap_or_default());
    if preference != models::SourcePreference::ModelScope {
        return Err("this probe requires ModelScope to be the effective saved/default source; it never changes preferences".into());
    }
    models::set_preference(preference, "");
    let file = "ggml-tiny.bin";
    let upstream = "https://huggingface.co/ggerganov/whisper.cpp/resolve/5359861c739e955e79d9a303bcbc70fb988958b1/ggml-tiny.bin";
    let digest = "be07e048e1e599ad46341c8d2a135645097a538221678b7acdd1b1919c6e1b21";
    let expected_bytes = 77_691_713;
    let sources = models::sources(file, upstream);
    let expected_source = models::modelscope(file).ok_or("missing verified ModelScope mapping")?;
    if sources.first() != Some(&expected_source) {
        return Err("ModelScope is not first".into());
    }
    log::set_logger(&RECEIPTS).map_err(|_| "could not set the receipt logger")?;
    log::set_max_level(log::LevelFilter::Info);
    let partial = output.join("ggml-tiny.bin.part");
    let cancel = AtomicBool::new(false);
    let started = std::time::Instant::now();
    let mut last = 0;
    let totals = models::fetch(
        file,
        upstream,
        &partial,
        expected_bytes,
        digest,
        &cancel,
        "cancelled",
        &mut |received, total| {
            if received >= last + 8 * 1024 * 1024 || received == total {
                eprintln!("{received} / {total}");
                last = received;
            }
        },
    )?;
    models::verify(&partial, digest)?;
    if std::fs::metadata(&partial)?.len() != expected_bytes {
        return Err("wrong file size".into());
    }
    let receipts = RECEIPTS.0.lock().expect("receipt log").clone();
    let confirmed_modelscope = receipts
        .iter()
        .any(|line| line == "models: verified ggml-tiny.bin from modelscope.cn");
    let destination = output.join(file);
    std::fs::rename(&partial, &destination)?;
    let result = json!({
        "production_downloader": "concat_host::models::fetch",
        "source_preference": preference.name(),
        "explicit_saved_choice": saved,
        "user_models_directory": dirs.data.join("whisper-models"),
        "isolated_destination": destination,
        "requested_source": expected_source,
        "verified_source_receipts": receipts,
        "received": totals.0, "total": totals.1,
        "sha256": digest, "sha256_verified": true,
        "seconds": started.elapsed().as_secs_f64(),
        "modelscope_success": confirmed_modelscope
    });
    let report = serde_json::to_string_pretty(&result)? + "\n";
    std::fs::write(output.join("native-download-result.json"), &report)?;
    println!("{report}");
    if !confirmed_modelscope {
        return Err("download succeeded via fallback, not ModelScope; see the receipt".into());
    }
    Ok(())
}
