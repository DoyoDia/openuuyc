//! AV1 bitstream syntax, low-delay encoder and software decoder.
//! Component provenance and licenses are retained under licenses/.
#![cfg_attr(target_arch = "arm", feature(stdarch_arm_feature_detection))]
#![cfg_attr(
    any(target_arch = "riscv32", target_arch = "riscv64"),
    feature(stdarch_riscv_feature_detection)
)]

#[cfg(test)]
#[macro_use]
extern crate pretty_assertions;

pub mod bitstream;
pub mod decoder;
pub mod encoder;
