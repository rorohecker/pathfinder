# Pathfinder 1.0.29

- **Display-refresh pacing.** Fantasy/sunset motion follows Auto / 60 / 120 / 144 / 165 / 240 Hz, capped to the panel, and re-probes when you drag the window to another monitor.
- **Hybrid CPU scheduling.** Background thumbs/index prefer efficiency cores + EcoQoS; interactive search/storage and ONNX CPU sessions prefer performance cores.
- **Smarter Local AI routing.** Sustained embeddings prefer NPU → iGPU → dGPU → CPU; interactive classify prefers dGPU → NPU → iGPU → CPU.
- **Skia renderer.** Prefer Skia with FemtoVG/software fallback; avoid ICU and MSVC CRT clashes that blocked the Windows link.
- **Safer paste / undo.** Folder→self/descendant paste is skipped without aborting the rest of a multi-selection; redo-copy uses the durable engine; redo-delete stays on the Recycle Bin.

Download `Pathfinder_1.0.29_x64-setup.exe` for the standard Windows installer, or `Pathfinder_1.0.29_x64_en-US.msi` for Windows Installer deployment.
