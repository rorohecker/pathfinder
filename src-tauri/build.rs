fn main() {
    slint_build::compile("ui/main.slint").expect("failed to compile Slint UI");
    tauri_build::build()
}
