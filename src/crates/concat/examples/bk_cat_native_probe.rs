// SPDX-License-Identifier: AGPL-3.0-or-later

fn main() {
    if let Err(error) = concat::agent::probe::run() {
        eprintln!("isolated native integration failed: {error}");
        std::process::exit(1);
    }
}
