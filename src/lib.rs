//! vgm2fur: transcribe YM2151/YM2203/YM2612 + sample-chip VGMs into Furnace
//! `.fur` modules. A Rust port of vgm2151fur 0.11.0.

pub mod analyze;
pub mod audit;
pub mod ay8910;
pub mod c140;
pub mod c219;
pub mod c352;
pub mod cli;
pub mod convert;
pub mod diag;
pub mod edit;
pub mod error;
pub mod es5503;
pub mod fds;
pub mod fm;
pub mod furio;
pub mod furnace;
pub mod furwrite;
pub mod ga20;
pub mod gb;
pub mod huc6280;
pub mod k005289;
pub mod k053260;
pub mod k054539;
pub mod levels;
pub mod loopfind;
pub mod menu;
pub mod mikey;
pub mod model;
pub mod msm5205;
pub mod msm5232;
pub mod multipcm;
pub mod nes;
pub mod oki;
pub mod opl;
pub mod packs;
pub mod parity;
pub mod paths;
pub mod pcm;
pub mod pitch;
pub mod pokey;
pub mod priority;
pub mod psg_dac;
pub mod pwm;
pub mod qsound;
pub mod rf5c;
pub mod saa1099;
pub mod samplecluster;
pub mod scc;
pub mod segapcm;
pub mod sn76489;
pub mod ssgpcm;
pub mod timing;
pub mod upd7759;
pub mod vgm;
pub mod vsu;
pub mod wonderswan;
pub mod x1_010;
pub mod y8950;
pub mod ym2413;
pub mod ym2608;
pub mod ym2203;
pub mod ym2610;
pub mod ym2612;
pub mod ym2612dac;
pub mod ymf278b;
pub mod ymz280b;

pub use error::{Error, Result};

/// The converter tag written into `INFO` and printed by `--version`. This is
/// the reference package version, not the crate version.
pub const CONVERTER_VERSION: &str = "0.11.0";
pub const CONVERTER_TAG: &str = "vgm2151fur 0.11.0";
