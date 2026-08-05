//! Per-request knob resolution: every override-able setting, resolved
//! exactly once at pipeline entry — per-call override, else `OXIMG_*`
//! env (the cached [`Config`]), else built-in default. Downstream
//! stages read resolved fields instead of re-deriving them, so the
//! precedence rule exists in one function instead of a resolver per
//! knob spread across three files. Config-only knobs (no per-call
//! tier) keep their direct `config()` reads — see the struct doc.

use super::{Encoder, Params, PngEffort};
use crate::config::Config;

/// AVIF encode knobs, resolved as a group because the alpha default
/// depends on the resolved color quality.
#[cfg(feature = "avif")]
#[derive(Clone, Copy, Debug)]
pub(crate) struct AvifKnobs {
    pub quality: u8,
    /// Already defaulted to the *resolved* color quality — the
    /// "alpha follows color" rule runs here, once, so no call site
    /// can get the ordering wrong.
    pub alpha_quality: u8,
    pub speed: i8,
}

/// One request's fully-resolved settings. Geometry fields carry over
/// from [`Params`] verbatim (same names, so call sites keep reading
/// `p.max_width`); knob fields hold the resolved value with no
/// `Option` left — except `png_effort`, whose *default* depends on
/// whether the encode quantizes, a fact known only mid-pipeline (see
/// [`Resolved::png_compression`]).
///
/// Config-only settings with no per-call override (`timing`,
/// `dct_margin`, `webp_effort`, `jpegli_progressive`, `fir_backend`,
/// decode threads, the source/pixel/byte caps) are deliberately not
/// here: they have no precedence to resolve and are read where they
/// apply.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Resolved {
    pub max_width: u32,
    pub max_height: u32,
    pub quality: f32,
    pub encoder: Encoder,
    pub parallel: usize,

    pub linear_light: bool,
    pub auto_rotate: bool,
    pub icc_passthrough: bool,
    pub flatten_bg: [u8; 3],
    pub webp_quality: f32,
    /// `Some(palette size, pre-clamped to 2..=256)` when quantization
    /// applies — the on/off flag and the size knob collapsed into one
    /// pre-validated value.
    pub png_quantize: Option<u16>,
    /// `None` = no explicit choice anywhere; the contextual default
    /// is applied by [`Resolved::png_compression`].
    png_effort: Option<png::Compression>,
    #[cfg(feature = "avif")]
    pub avif: AvifKnobs,
}

impl Resolved {
    /// The one doorway from the public [`Params`] to internal
    /// settings — the only per-request `config()` consumer.
    pub(crate) fn new(p: &Params) -> Resolved {
        Self::with_config(p, crate::config::config())
    }

    /// Pure function of `(Params, Config)`: no env, no OnceLock, no
    /// I/O — the unit-testable core. Precedence for every knob:
    /// per-call override, else config, else built-in default (the
    /// config carries the built-in defaults already).
    pub(crate) fn with_config(p: &Params, cfg: &Config) -> Resolved {
        #[cfg(feature = "avif")]
        let avif = {
            let quality = p.avif_quality.unwrap_or(cfg.avif_quality);
            AvifKnobs {
                quality,
                alpha_quality: cfg.avif_alpha_quality.unwrap_or(quality),
                speed: cfg.avif_speed,
            }
        };
        Resolved {
            max_width: p.max_width,
            max_height: p.max_height,
            quality: p.quality,
            encoder: p.encoder,
            parallel: p.parallel,
            linear_light: p.linear_light.unwrap_or(cfg.linear_light),
            auto_rotate: p.auto_rotate.unwrap_or(cfg.auto_rotate),
            icc_passthrough: p.icc.unwrap_or(cfg.icc_passthrough),
            flatten_bg: p.flatten_bg.unwrap_or(cfg.flatten_bg),
            webp_quality: p.webp_quality.unwrap_or(cfg.webp_quality),
            png_quantize: {
                let on = p.png_quantize.unwrap_or(cfg.png_quantize);
                let colors = p
                    .png_quantize_colors
                    .unwrap_or(cfg.png_quantize_colors)
                    .clamp(2, 256);
                on.then_some(colors)
            },
            png_effort: match p.png_effort {
                Some(PngEffort::Fastest) => Some(png::Compression::Fastest),
                Some(PngEffort::Fast) => Some(png::Compression::Fast),
                Some(PngEffort::Balanced) => Some(png::Compression::Balanced),
                Some(PngEffort::High) => Some(png::Compression::High),
                None => cfg.png_compression,
            },
            #[cfg(feature = "avif")]
            avif,
        }
    }

    /// The one knob that cannot flatten at resolve time: with no
    /// explicit effort anywhere, `balanced` nearly doubles the byte
    /// reduction over `fast` when quantizing (1.7x -> 3.0x against
    /// lossless) while the lossless default stays `fast`, where effort
    /// buys much less. Whether *this* encode quantizes is known only
    /// at the encoder (alpha sources skip quantization).
    pub(crate) fn png_compression(&self, quantizing: bool) -> png::Compression {
        self.png_effort.unwrap_or(if quantizing {
            png::Compression::Balanced
        } else {
            png::Compression::Fast
        })
    }

    /// The tuned AVIF operating point (see QUALITY.md), built from the
    /// resolved knobs — shared by the one-shot encoder and the fused
    /// workers' preheated sessions.
    #[cfg(feature = "avif")]
    pub(crate) fn avif_params(&self) -> crate::avif::AvifParams {
        crate::avif::AvifParams {
            quality: self.avif.quality,
            alpha_quality: self.avif.alpha_quality,
            speed: self.avif.speed,
            ..Default::default()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A Config at the documented defaults — hand-built, so these
    /// tests never read the environment and stay parallel-safe.
    fn base_cfg() -> Config {
        Config {
            timing: false,
            linear_light: true,
            fir_backend: false,
            auto_rotate: true,
            icc_passthrough: true,
            dct_margin: 1.7,
            jpegli_progressive: true,
            flatten_bg: [255, 255, 255],
            png_compression: None,
            png_quantize: false,
            png_quantize_colors: 256,
            webp_quality: 75.0,
            webp_effort: 2,
            webp_decode_threads: true,
            #[cfg(feature = "avif")]
            avif_quality: 55,
            #[cfg(feature = "avif")]
            avif_alpha_quality: None,
            #[cfg(feature = "avif")]
            avif_speed: 8,
            #[cfg(feature = "avif")]
            avif_decode_threads: 1,
            max_source_bytes: 64 * 1024 * 1024,
            upstream_connect_timeout: 5,
            upstream_timeout: 30,
            max_decoded_bytes: None,
            log_decoded_bytes_above: None,
            max_src_pixels: 64_000_000,
        }
    }

    /// With every override unset, each knob resolves to the config
    /// value — the documented defaults, pinned without env reads.
    #[test]
    fn defaults_resolve_from_config() {
        let r = Resolved::with_config(&Params::default(), &base_cfg());
        assert!(r.linear_light && r.auto_rotate && r.icc_passthrough);
        assert_eq!(r.flatten_bg, [255, 255, 255]);
        assert_eq!(r.webp_quality, 75.0);
        assert_eq!(r.png_quantize, None);
        assert!(r.png_effort.is_none());
        #[cfg(feature = "avif")]
        {
            assert_eq!(r.avif.quality, 55);
            assert_eq!(r.avif.alpha_quality, 55);
            assert_eq!(r.avif.speed, 8);
        }
    }

    /// Every per-call override beats a config that disagrees with it —
    /// the table the ten deleted resolver fns each asserted one row of.
    #[test]
    fn overrides_beat_config() {
        let mut cfg = base_cfg();
        cfg.linear_light = false;
        cfg.auto_rotate = false;
        cfg.icc_passthrough = false;
        cfg.flatten_bg = [0, 0, 0];
        cfg.webp_quality = 10.0;
        cfg.png_quantize = false;
        cfg.png_quantize_colors = 16;
        cfg.png_compression = Some(png::Compression::High);
        #[cfg(feature = "avif")]
        {
            cfg.avif_quality = 10;
        }
        let p = Params {
            linear_light: Some(true),
            auto_rotate: Some(true),
            icc: Some(true),
            flatten_bg: Some([1, 2, 3]),
            webp_quality: Some(40.0),
            png_quantize: Some(true),
            png_quantize_colors: Some(128),
            png_effort: Some(PngEffort::Fastest),
            #[cfg(feature = "avif")]
            avif_quality: Some(60),
            ..Params::default()
        };
        let r = Resolved::with_config(&p, &cfg);
        assert!(r.linear_light && r.auto_rotate && r.icc_passthrough);
        assert_eq!(r.flatten_bg, [1, 2, 3]);
        assert_eq!(r.webp_quality, 40.0);
        assert_eq!(r.png_quantize, Some(128));
        assert!(matches!(r.png_effort, Some(png::Compression::Fastest)));
        #[cfg(feature = "avif")]
        assert_eq!(r.avif.quality, 60);
    }

    /// `Some(env default)` and `None` resolve identically — the
    /// contract tests/params_overrides.rs pins end-to-end, here at
    /// the unit level.
    #[test]
    fn some_default_equals_none() {
        let cfg = base_cfg();
        let explicit = Params {
            webp_quality: Some(75.0),
            linear_light: Some(true),
            ..Params::default()
        };
        let a = Resolved::with_config(&explicit, &cfg);
        let b = Resolved::with_config(&Params::default(), &cfg);
        assert_eq!(a.webp_quality, b.webp_quality);
        assert_eq!(a.linear_light, b.linear_light);
    }

    /// The alpha default follows the *resolved* color quality: from
    /// the override when set, from config otherwise — and an explicit
    /// alpha config still wins over both.
    #[cfg(feature = "avif")]
    #[test]
    fn avif_alpha_follows_resolved_color_quality() {
        let cfg = base_cfg();
        let p = Params {
            avif_quality: Some(30),
            ..Params::default()
        };
        assert_eq!(Resolved::with_config(&p, &cfg).avif.alpha_quality, 30);
        assert_eq!(
            Resolved::with_config(&Params::default(), &cfg)
                .avif
                .alpha_quality,
            55
        );
        let mut cfg = base_cfg();
        cfg.avif_alpha_quality = Some(90);
        assert_eq!(Resolved::with_config(&p, &cfg).avif.alpha_quality, 90);
    }

    /// The contextual PNG default: quantizing selects `Balanced`,
    /// lossless selects `Fast`, and any explicit effort (per-call or
    /// env) wins in both contexts.
    #[test]
    fn png_compression_contextual_default() {
        let cfg = base_cfg();
        let unset = Resolved::with_config(&Params::default(), &cfg);
        assert!(matches!(
            unset.png_compression(false),
            png::Compression::Fast
        ));
        assert!(matches!(
            unset.png_compression(true),
            png::Compression::Balanced
        ));
        let explicit = Resolved::with_config(
            &Params {
                png_effort: Some(PngEffort::High),
                ..Params::default()
            },
            &cfg,
        );
        assert!(matches!(
            explicit.png_compression(false),
            png::Compression::High
        ));
        assert!(matches!(
            explicit.png_compression(true),
            png::Compression::High
        ));
        let mut cfg_env = base_cfg();
        cfg_env.png_compression = Some(png::Compression::Fastest);
        let env = Resolved::with_config(&Params::default(), &cfg_env);
        assert!(matches!(
            env.png_compression(true),
            png::Compression::Fastest
        ));
    }

    /// Palette size is pre-clamped to what PNG's PLTE can hold.
    #[test]
    fn png_quantize_colors_clamped() {
        let cfg = base_cfg();
        let p = Params {
            png_quantize: Some(true),
            png_quantize_colors: Some(1),
            ..Params::default()
        };
        assert_eq!(Resolved::with_config(&p, &cfg).png_quantize, Some(2));
        let p = Params {
            png_quantize: Some(true),
            png_quantize_colors: Some(1000),
            ..Params::default()
        };
        assert_eq!(Resolved::with_config(&p, &cfg).png_quantize, Some(256));
    }

    /// All whitespace removed, so a needle still matches source that
    /// rustfmt has broken across lines.
    fn squeeze(s: &str) -> String {
        s.chars().filter(|c| !c.is_whitespace()).collect()
    }

    /// The resolve-once rule, pinned so it survives the next knob.
    ///
    /// Two nets. Every `Option` field of [`Params`] is an override
    /// tier, so this file must read it: add a field, forget the
    /// `Resolved` line, and the override would be silently ignored at
    /// runtime with every existing test still green. And the one-line
    /// resolver idiom the ten deleted helpers shared must not come
    /// back in a stage — that copy-paste is how the duplication grew
    /// the first time. Direct config-only reads (`config().timing`,
    /// `webp_effort()`, the caps) are untouched by either net: they
    /// have no per-call tier to merge.
    #[test]
    fn every_override_resolves_here() {
        let this = squeeze(include_str!("resolved.rs"));
        let mod_rs = include_str!("mod.rs");
        // The `Params` declaration alone: other structs in mod.rs
        // carry `Option` fields that are not knobs.
        let decl = mod_rs
            .split_once("pub struct Params {")
            .expect("Params declaration")
            .1
            .split_once("\n}\n")
            .expect("end of the Params declaration")
            .0;
        let mut checked = 0;
        for line in decl.lines() {
            let Some(name) = line
                .trim()
                .strip_prefix("pub ")
                .and_then(|f| f.split_once(": Option<"))
                .map(|(name, _)| name)
            else {
                continue;
            };
            // `output` is the target format, not a knob: it selects
            // the encoder path before resolution and has no env tier.
            if name == "output" {
                continue;
            }
            assert!(
                this.contains(&format!("p.{name}")),
                "Params::{name} is a per-call override that Resolved::with_config never reads"
            );
            checked += 1;
        }
        // The scan itself must not silently find nothing.
        assert!(checked >= 8, "only {checked} override fields found");

        for stage in [
            include_str!("mod.rs"),
            include_str!("encode.rs"),
            include_str!("formats.rs"),
            include_str!("jpeg.rs"),
            include_str!("fuse.rs"),
        ] {
            for idiom in [
                "unwrap_or_else(||crate::config::config()",
                "unwrap_or(crate::config::config()",
            ] {
                assert!(
                    !squeeze(stage).contains(idiom),
                    "a per-knob resolver is back; merge the knob in Resolved::with_config instead"
                );
            }
        }
    }
}
