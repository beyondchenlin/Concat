// SPDX-License-Identifier: AGPL-3.0-or-later

fn main() {
    // Preserve the window build's stack and resource policy while compiling
    // the generated Rust in a separate crate.
    std::thread::Builder::new()
        .name("slint".into())
        .stack_size(512 << 20)
        .spawn(|| {
            let config = slint_build::CompilerConfiguration::new()
                .embed_resources(slint_build::EmbedResourcesKind::EmbedFiles);
            slint_build::compile_with_config("../concat/ui/app.slint", config)
        })
        .expect("could not start the Slint compiler thread")
        .join()
        .expect("the Slint compiler thread panicked")
        .expect("failed to compile ui/app.slint");
}
