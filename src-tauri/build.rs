fn main() {
    // Embed Press Start 2P (OFL) so the Retro theme can use a true pixel-arcade font
    // even on machines that don't have an arcade-style font installed.
    slint_build::compile_with_config(
        "ui/main.slint",
        slint_build::CompilerConfiguration::new().embed_resources(
            slint_build::EmbedResourcesKind::EmbedFiles,
        ),
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
    tauri_build::build()
}
