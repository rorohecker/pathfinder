fn main() {
    // Embed Press Start 2P (OFL) so the Retro theme can use a true pixel-arcade font
    // even on machines that don't have an arcade-style font installed.
    // Bundled gettext .po files live under lang/<lang>/LC_MESSAGES/pathfinder.po
    // and are selected at runtime with slint::select_bundled_translation.
    slint_build::compile_with_config(
        "ui/main.slint",
        slint_build::CompilerConfiguration::new()
            .embed_resources(slint_build::EmbedResourcesKind::EmbedFiles)
            .with_bundled_translations("lang")
            // Keep .po files simple: English source text is the key (no component msgctxt).
            .with_default_translation_context(slint_build::DefaultTranslationContext::None),
    )
    .expect("failed to compile Slint UI");
    println!("cargo:rerun-if-changed=ui/fantasy_fx.slint");
    println!("cargo:rerun-if-changed=ui/fantasy_icons.slint");
    println!("cargo:rerun-if-changed=ui/retro_fx.slint");
    println!("cargo:rerun-if-changed=ui/retro_icons.slint");
    println!("cargo:rerun-if-changed=ui/sunset_fx.slint");
    println!("cargo:rerun-if-changed=ui/sunset_icons.slint");
    println!("cargo:rerun-if-changed=ui/fonts/PressStart2P-Regular.ttf");
    println!("cargo:rerun-if-changed=ui/fonts/NotoSans-Regular.ttf");
    println!("cargo:rerun-if-changed=ui/fonts/NotoSansMono-Regular.ttf");
    println!("cargo:rerun-if-changed=ui/fonts/JetBrainsMono-Variable.ttf");
    println!("cargo:rerun-if-changed=ui/fonts/Inter-Regular.ttf");
    println!("cargo:rerun-if-changed=ui/fonts/Lora-Regular.ttf");
    println!("cargo:rerun-if-changed=ui/fonts/FiraCode-Regular.ttf");
    println!("cargo:rerun-if-changed=lang");
    tauri_build::build()
}
