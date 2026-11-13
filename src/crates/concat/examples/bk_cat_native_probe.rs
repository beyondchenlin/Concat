// SPDX-License-Identifier: AGPL-3.0-or-later
//! Feature-gated executable for isolated native, platform and document checks.

fn main() {
    let args: Vec<_> = std::env::args().skip(1).collect();
    let result = if args.windows(2).any(|pair| pair == ["--mode", "script"]) {
        concat::agent::script_probe::run()
    } else {
        concat::agent::probe::run()
    };
    if let Err(error) = result {
        eprintln!("isolated native integration failed: {error}");
        std::process::exit(1);
    }
}
