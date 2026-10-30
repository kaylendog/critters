//! The taste model. `embed` turns images into CLIP vectors, `rank` learns a
//! preference direction over them from labelled pairs.

pub mod embed;
pub mod rank;
