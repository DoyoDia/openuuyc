//! One build entry; encoder/decoder assembly has isolated generated files.
#[cfg(feature = "asm")]
#[path = "build/decoder.rs"]
mod decoder;
#[cfg(feature = "asm")]
#[path = "build/encoder.rs"]
mod encoder;

fn main() {
    #[cfg(feature = "asm")]
    {
        // Includes/macros are build inputs too. Cargo owns incremental reuse;
        // a second partial hash would miss edits to an included assembly file.
        println!("cargo:rerun-if-changed=src/encoder");
        println!("cargo:rerun-if-changed=src/decoder");
        encoder::build();
        decoder::build();
    }
}
