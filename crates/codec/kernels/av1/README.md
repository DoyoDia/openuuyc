# OpenUUYC AV1 kernel

One Rust crate and one build entry for the AV1 implementation:

- `src/bitstream`: OBU metadata, sequence headers and hardware decode syntax.
- `src/encoder`: immediate-output 4:2:0 / 4:4:4, 8 / 10-bit Rust encoder.
- `src/decoder`: software decoder and owned-pixel packing for the same formats.
- `build.rs`: builds the encoder and decoder SIMD assembly with separate
  generated configuration, object and archive directories. Cargo tracks included
  assembly files as build inputs; disabling `asm` omits NASM/CC build dependencies.

These are modules, not separately built or published crates. The application
uses them through `openuuyc-codec`; no future-frame queue is introduced.
The product retains the existing opt-level 3, one codegen unit and no LTO.

Encoding derives from rav1e revision
`31435de9d76fddd38f6dcc31d4014574cebb2092`; decoding derives from rav1d revision
`d3d1cd67059f47803919be8276650e5870c9fd02`. Bitstream parsing includes Chromium
and dav1d-derived code. Original copyright notices remain in the source;
licenses and patent grants are under `licenses/`.

The streaming subset keeps key/P output, live bitrate/FPS changes, cooperative
cancellation and keyframe recovery. Film grain, future-frame analysis, file
two-pass processing, standalone CLI and configuration serialization are excluded.
Zero lookahead does not promise a particular frame rate on every CPU.
