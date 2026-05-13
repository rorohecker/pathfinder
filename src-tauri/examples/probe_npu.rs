// Standalone probe: run with `cargo run --example probe_npu` to print every
// device the NPU detector finds on this machine. Useful for debugging cases
// where the AI tab reports CPU-only despite Windows seeing an NPU.
fn main() {
    pathfinder_lib::__test_detect_npus_verbose();
    println!();
    let npus = pathfinder_lib::__test_detect_npus();
    println!("detect_npus() returned {} device(s):", npus.len());
    for (i, name) in npus.iter().enumerate() {
        println!("  [{}] {}", i, name);
    }
    let gpus = pathfinder_lib::__test_detect_gpus();
    println!("detect_gpus() returned {} adapter(s):", gpus.len());
    for (i, gpu) in gpus.iter().enumerate() {
        println!(
            "  [{}] {} (vendor={}, dedicated_video_mb={}, hardware={}, discrete={})",
            i, gpu.0, gpu.1, gpu.2, gpu.3, gpu.4
        );
    }
}
