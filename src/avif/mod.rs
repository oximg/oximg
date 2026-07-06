//! AVIF encoding via SVT-AV1 (sync-path settings validated in the encoder
//! study: preset 8, tune=3, 10-bit 4:2:0) and decoding via dav1d. The SVT
//! session setup mirrors libavif's codec_svt.c so measurements against
//! `avifenc -c svt` transfer.

use crate::svt::bindings as svt;
use crate::yuv::{self, Row};
use anyhow::{Context, Result, ensure};

/// libavif's quality -> quantizer mapping (codec_svt.c).
mod decode;
mod encode;
mod isobmff;
mod rgb2yuv;
#[cfg(test)]
mod tests;

pub use decode::{decode_avif, decode_avif_into, probe_avif};
pub use encode::{AvifParams, encode_avif};
pub(crate) use encode::{
    SvtSession, encode_avif_rgb_with_session, encode_avif_with_session, start_color_session,
};
pub use isobmff::extract_icc;
#[cfg_attr(not(test), allow(unused_imports))]
use isobmff::*;
pub(crate) use isobmff::{embed_icc, extract_orientation};
#[cfg_attr(not(test), allow(unused_imports))]
use rgb2yuv::*;
pub(crate) use rgb2yuv::{chroma_row_pair, luma_rows};
