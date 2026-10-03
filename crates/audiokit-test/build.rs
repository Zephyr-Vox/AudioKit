//! The CLI has no Slint build dependency unless the GUI feature is selected.
fn main() {
    #[cfg(feature = "gui")]
    slint_build::compile_with_config(
        "ui/workbench.slint",
        slint_build::CompilerConfiguration::new()
            .with_style("fluent".into())
            .embed_resources(slint_build::EmbedResourcesKind::EmbedFiles),
    )
    .expect("compile AudioKit workbench");
}
