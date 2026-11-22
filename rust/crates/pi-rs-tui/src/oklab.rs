//! Oklab and OKHSL <-> sRGB conversion.
//!
//! Ported from `packages/tui/src/oklab.ts`.
//!
//! Oklab and OKHSL are Björn Ottosson's color spaces; OKHSL's saturation is relative to the sRGB gamut at
//! each hue and lightness. This is a port of his reference implementation, Copyright (c) 2021 Björn
//! Ottosson, used under the MIT license:
//!
//! Permission is hereby granted, free of charge, to any person obtaining a copy of this software and
//! associated documentation files (the "Software"), to deal in the Software without restriction, including
//! without limitation the rights to use, copy, modify, merge, publish, distribute, sublicense, and/or sell
//! copies of the Software, and to permit persons to whom the Software is furnished to do so, subject to
//! the following conditions: The above copyright notice and this permission notice shall be included in all
//! copies or substantial portions of the Software. THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY
//! KIND, EXPRESS OR IMPLIED.

use crate::colors::{OkhslChannels, RgbColorValue};

/// A three component vector.
type Vector = [f64; 3];

/// A 3x3 matrix, stored row by row.
type Matrix = [[f64; 3]; 3];

const LINEAR_SRGB_TO_LMS: Matrix = [
    [0.4122214694707629, 0.5363325372617349, 0.0514459932675022],
    [0.2119034958178251, 0.6806995506452344, 0.1073969535369405],
    [0.0883024591900564, 0.2817188391361215, 0.6299787016738222],
];
const LMS_TO_LAB: Matrix = [
    [0.210454268309314, 0.793617774702305, -0.0040720430116193],
    [1.9779985324311684, -2.42859224204858, 0.450593709617411],
    [0.0259040424655478, 0.7827717124575296, -0.8086757549230774],
];
const LAB_TO_LMS: Matrix = [
    [1.0, 0.3963377773761749, 0.2158037573099136],
    [1.0, -0.1055613458156586, -0.0638541728258133],
    [1.0, -0.0894841775298119, -1.2914855480194092],
];
const LMS_TO_LINEAR_SRGB: Matrix = [
    [4.076741636075958, -3.307711539258063, 0.2309699031821043],
    [-1.2684379732850315, 2.609757349287688, -0.341319376002657],
    [-0.0041960761386756, -0.7034186179359362, 1.7076146940746117],
];
/// Per sRGB channel (red, green, blue): the (a, b) half-plane where that channel clips first, and the
/// polynomial approximating the maximum saturation there.
const SATURATION_FIT: [([f64; 2], [f64; 5]); 3] = [
    (
        [-1.8817031, -0.80936501],
        [1.19086277, 1.76576728, 0.59662641, 0.75515197, 0.56771245],
    ),
    (
        [1.8144408, -1.19445267],
        [0.73956515, -0.45954404, 0.08285427, 0.12541073, -0.14503204],
    ),
    (
        [0.13110758, 1.81333971],
        [1.35733652, -0.00915799, -1.1513021, -0.50559606, 0.00692167],
    ),
];
const K1: f64 = 0.206;
const K2: f64 = 0.03;
const K3: f64 = (1.0 + K1) / (1.0 + K2);

/// Rounds half towards positive infinity, like JavaScript's `Math.round`, and never returns `-0`.
pub(crate) fn js_round(value: f64) -> f64 {
    let rounded = (value + 0.5).floor();
    if rounded == 0.0 { 0.0 } else { rounded }
}

fn multiply(m: &Matrix, v: &Vector) -> Vector {
    [
        m[0][0] * v[0] + m[0][1] * v[1] + m[0][2] * v[2],
        m[1][0] * v[0] + m[1][1] * v[1] + m[1][2] * v[2],
        m[2][0] * v[0] + m[2][1] * v[1] + m[2][2] * v[2],
    ]
}

/// Oklab lightness to OKHSL lightness.
pub fn oklab_to_okhsl_lightness(x: f64) -> f64 {
    0.5 * (K3 * x - K1 + ((K3 * x - K1).powf(2.0) + 4.0 * K2 * K3 * x).sqrt())
}

/// OKHSL lightness to Oklab lightness.
fn okhsl_to_oklab_lightness(x: f64) -> f64 {
    (x * x + K1 * x) / (K3 * (x + K2))
}

/// sRGB transfer function: linear to encoded channel, both 0-1.
fn linear_to_srgb(value: f64) -> f64 {
    if value > 0.0031308 {
        1.055 * value.powf(1.0 / 2.4) - 0.055
    } else {
        12.92 * value
    }
}

/// Inverse sRGB transfer function: encoded to linear channel, both 0-1.
fn srgb_to_linear(value: f64) -> f64 {
    if value <= 0.04045 {
        value / 12.92
    } else {
        ((value + 0.055) / 1.055).powf(2.4)
    }
}

/// Oklab `[L, a, b]` to linear sRGB `[r, g, b]` (0-1, may leave the gamut).
pub fn oklab_to_linear_srgb(lab: Vector) -> Vector {
    let lms = multiply(&LAB_TO_LMS, &lab);
    let cubes = [lms[0].powf(3.0), lms[1].powf(3.0), lms[2].powf(3.0)];
    multiply(&LMS_TO_LINEAR_SRGB, &cubes)
}

/// FreeBSD / SunPro `s_cbrt.c` cube root matching V8's `Math.cbrt` bit-for-bit
/// without pinning `libm = "=0.2.11"` against newer workspace dependencies.
///
/// Origin: FreeBSD `/usr/src/lib/msun/src/s_cbrt.c` (Copyright (C) 1993 by Sun Microsystems,
/// Inc. All rights reserved. Developed at SunPro, a Sun Microsystems, Inc. business.
/// Permission to use, copy, modify, and distribute this software is freely granted, provided
/// that this notice is preserved. Optimized by Bruce D. Evans.)
fn cbrt(x: f64) -> f64 {
    const B1: u32 = 715_094_163;
    const B2: u32 = 696_219_795;
    const P0: f64 = f64::from_bits(0x3ffe_03e6_0f61_e692);
    const P1: f64 = f64::from_bits(0xbffe_28e0_92f0_2420);
    const P2: f64 = f64::from_bits(0x3ff9_f160_4a49_d6c2);
    const P3: f64 = f64::from_bits(0xbfe8_44cb_bee7_51d9);
    const P4: f64 = f64::from_bits(0x3fc2_b000_d4e4_edd7);

    let x1p54 = f64::from_bits(0x4350_0000_0000_0000);
    let mut ui: u64 = x.to_bits();
    let mut hx: u32 = (ui >> 32) as u32 & 0x7fff_ffff;
    if hx >= 0x7ff0_0000 {
        return x + x;
    }
    if hx < 0x0010_0000 {
        ui = (x * x1p54).to_bits();
        hx = (ui >> 32) as u32 & 0x7fff_ffff;
        if hx == 0 {
            return x;
        }
        hx = hx / 3 + B2;
    } else {
        hx = hx / 3 + B1;
    }
    ui &= 1 << 63;
    ui |= u64::from(hx) << 32;
    let mut t = f64::from_bits(ui);

    let mut r = (t * t) * (t / x);
    t *= (P0 + r * (P1 + r * P2)) + ((r * r) * r) * (P3 + r * P4);

    ui = t.to_bits();
    ui = (ui + 0x8000_0000) & 0xffff_ffff_c000_0000;
    t = f64::from_bits(ui);

    let s = t * t;
    r = x / s;
    let w = t + t;
    r = (r - t) / (w + r);
    t + t * r
}

/// Linear sRGB `[r, g, b]` to Oklab `[L, a, b]`.
fn linear_srgb_to_oklab(rgb: Vector) -> Vector {
    let lms = multiply(&LINEAR_SRGB_TO_LMS, &rgb);
    let cubes = lms.map(cbrt);
    multiply(&LMS_TO_LAB, &cubes)
}

/// sRGB channels (0-255) to Oklab `[L, a, b]`.
pub fn rgb_to_oklab(rgb: &RgbColorValue) -> Vector {
    let encoded = [rgb.r / 255.0, rgb.g / 255.0, rgb.b / 255.0];
    let linear = [
        srgb_to_linear(encoded[0]),
        srgb_to_linear(encoded[1]),
        srgb_to_linear(encoded[2]),
    ];
    linear_srgb_to_oklab(linear)
}

/// Linear sRGB `[r, g, b]` to sRGB channels (0-255, rounded), clipping out-of-gamut channels.
pub fn linear_srgb_to_rgb(linear: Vector) -> RgbColorValue {
    let channel = |value: f64| js_round(linear_to_srgb(value).clamp(0.0, 1.0) * 255.0);
    RgbColorValue {
        r: channel(linear[0]),
        g: channel(linear[1]),
        b: channel(linear[2]),
    }
}

/// Rate of change of each cube-root LMS component along a chroma direction (a, b).
fn lms_slopes(a: f64, b: f64) -> Vector {
    [
        LAB_TO_LMS[0][1] * a + LAB_TO_LMS[0][2] * b,
        LAB_TO_LMS[1][1] * a + LAB_TO_LMS[1][2] * b,
        LAB_TO_LMS[2][1] * a + LAB_TO_LMS[2][2] * b,
    ]
}

/// Largest saturation (C/L) inside sRGB for hue (a, b): polynomial fit plus one Halley step.
fn max_saturation(a: f64, b: f64) -> f64 {
    let channel = SATURATION_FIT
        .iter()
        .enumerate()
        .position(|(index, ([x, y], _))| index == 2 || x * a + y * b > 1.0)
        .unwrap_or(SATURATION_FIT.len() - 1);
    let [k0, k1, k2, k3, k4] = SATURATION_FIT[channel].1;
    let weights = LMS_TO_LINEAR_SRGB[channel];
    let saturation = k0 + k1 * a + k2 * b + k3 * a * a + k4 * a * b;

    let slopes = lms_slopes(a, b);
    let base = [
        1.0 + saturation * slopes[0],
        1.0 + saturation * slopes[1],
        1.0 + saturation * slopes[2],
    ];
    let dot =
        |values: [f64; 3]| weights[0] * values[0] + weights[1] * values[1] + weights[2] * values[2];
    let f = dot([base[0].powf(3.0), base[1].powf(3.0), base[2].powf(3.0)]);
    let f1 = dot([
        3.0 * slopes[0] * base[0].powf(2.0),
        3.0 * slopes[1] * base[1].powf(2.0),
        3.0 * slopes[2] * base[2].powf(2.0),
    ]);
    let f2 = dot([
        6.0 * slopes[0].powf(2.0) * base[0],
        6.0 * slopes[1].powf(2.0) * base[1],
        6.0 * slopes[2].powf(2.0) * base[2],
    ]);
    saturation - (f * f1) / (f1 * f1 - 0.5 * f * f2)
}

/// Oklab lightness and chroma of the most saturated sRGB color of hue (a, b).
fn cusp(a: f64, b: f64) -> (f64, f64) {
    let saturation = max_saturation(a, b);
    let linear = oklab_to_linear_srgb([1.0, saturation * a, saturation * b]);
    let largest = linear[0].max(linear[1]).max(linear[2]);
    let lightness = cbrt(1.0 / largest);
    (lightness, lightness * saturation)
}

/// Chroma where the constant-lightness line at `lightness` leaves the sRGB gamut.
fn max_chroma(a: f64, b: f64, lightness: f64, peak: (f64, f64)) -> f64 {
    let (cusp_l, cusp_c) = peak;
    if lightness <= cusp_l {
        return (cusp_c * lightness) / cusp_l;
    }
    // Upper half: triangle edge, then one Halley step against each channel reaching 1.
    let t = (cusp_c * (lightness - 1.0)) / (cusp_l - 1.0);
    let slopes = lms_slopes(a, b);
    let lms = [
        lightness + t * slopes[0],
        lightness + t * slopes[1],
        lightness + t * slopes[2],
    ];
    let cubes = [lms[0].powf(3.0), lms[1].powf(3.0), lms[2].powf(3.0)];
    let first = [
        3.0 * slopes[0] * lms[0].powf(2.0),
        3.0 * slopes[1] * lms[1].powf(2.0),
        3.0 * slopes[2] * lms[2].powf(2.0),
    ];
    let second = [
        6.0 * slopes[0].powf(2.0) * lms[0],
        6.0 * slopes[1].powf(2.0) * lms[1],
        6.0 * slopes[2].powf(2.0) * lms[2],
    ];
    let dot = |row: [f64; 3], values: [f64; 3]| {
        row[0] * values[0] + row[1] * values[1] + row[2] * values[2]
    };
    let mut steps = [f64::MAX; 3];
    for (index, row) in LMS_TO_LINEAR_SRGB.iter().enumerate() {
        let f = dot(*row, cubes) - 1.0;
        let f1 = dot(*row, first);
        let f2 = dot(*row, second);
        let u = f1 / (f1 * f1 - 0.5 * f * f2);
        steps[index] = if u >= 0.0 { -f * u } else { f64::MAX };
    }
    t + steps[0].min(steps[1]).min(steps[2])
}

/// Match the scaled, two-channel `Math.hypot` calculation used by the oracle.
pub(crate) fn chroma(a: f64, b: f64) -> f64 {
    let (a, b) = (a.abs(), b.abs());
    if a.is_infinite() || b.is_infinite() {
        return f64::INFINITY;
    }
    if a.is_nan() || b.is_nan() {
        return f64::NAN;
    }
    let scale = a.max(b);
    if scale == 0.0 {
        return 0.0;
    }
    let (a, b) = (a / scale, b / scale);
    (a * a + b * b).sqrt() * scale
}

/// OKHSL's chroma reference points at lightness L and hue (a, b): `[c0, cMid, cMax]`.
fn chroma_stops(l: f64, a: f64, b: f64) -> (f64, f64, f64) {
    let peak = cusp(a, b);
    let c_max = max_chroma(a, b, l, peak);
    let k = c_max / (l * (peak.1 / peak.0)).min((1.0 - l) * (peak.1 / (1.0 - peak.0)));
    let mid_s = 0.11516993
        + 1.0
            / (7.4477897
                + 4.1590124 * b
                + a * (-2.19557347
                    + 1.75198401 * b
                    + a * (-2.13704948 - 10.02301043 * b
                        + a * (-4.24894561 + 5.38770819 * b + 4.69891013 * a))));
    let mid_t = 0.11239642
        + 1.0
            / (1.6132032 - 0.68124379 * b
                + a * (0.40370612
                    + 0.90148123 * b
                    + a * (-0.27087943
                        + 0.6122399 * b
                        + a * (0.00299215 - 0.45399568 * b - 0.14661872 * a))));
    let c_mid = 0.9
        * k
        * (1.0 / (1.0 / (l * mid_s).powf(4.0) + 1.0 / ((1.0 - l) * mid_t).powf(4.0)))
            .sqrt()
            .sqrt();
    let c0 = (1.0 / (1.0 / (l * 0.4).powf(2.0) + 1.0 / ((1.0 - l) * 0.8).powf(2.0))).sqrt();
    (c0, c_mid, c_max)
}

/// Convert OKHSL to sRGB channels (0-255, rounded), clipping out-of-gamut channels.
///
/// `hue` is in degrees, `saturation` and `lightness` are 0-1.
pub fn okhsl_to_rgb(hue: f64, saturation: f64, lightness: f64) -> RgbColorValue {
    let l = okhsl_to_oklab_lightness(lightness);
    let mut lab = [l, 0.0, 0.0];
    if l > 0.0 && l < 1.0 && saturation > 0.0 {
        let angle = (2.0 * std::f64::consts::PI * (((hue % 360.0) + 360.0) % 360.0)) / 360.0;
        let a = angle.cos();
        let b = angle.sin();
        let (c0, c_mid, c_max) = chroma_stops(l, a, b);
        // Chroma rises from 0 through cMid at s = 0.8 to cMax at s = 1.
        let chroma = if saturation < 0.8 {
            let t = 1.25 * saturation;
            let k1 = 0.8 * c0;
            (t * k1) / (1.0 - (1.0 - k1 / c_mid) * t)
        } else {
            let t = 5.0 * (saturation - 0.8);
            let k1 = (0.2 * c_mid.powf(2.0) * 1.25_f64.powf(2.0)) / c0;
            c_mid + (t * k1) / (1.0 - (1.0 - k1 / (c_max - c_mid)) * t)
        };
        lab = [l, chroma * a, chroma * b];
    }
    linear_srgb_to_rgb(oklab_to_linear_srgb(lab))
}

/// Convert sRGB channels (0-255) to OKHSL.
///
/// Returns the hue in degrees (0 for grays) and the saturation and lightness, both 0-1.
pub fn rgb_to_okhsl(rgb: &RgbColorValue) -> OkhslChannels {
    let [l, lab_a, lab_b] = rgb_to_oklab(rgb);
    let chroma = chroma(lab_a, lab_b);
    let lightness = oklab_to_okhsl_lightness(l);
    if chroma < 1e-9 || lightness <= 0.0 || lightness >= 1.0 {
        return OkhslChannels {
            h: 0.0,
            s: 0.0,
            l: lightness,
        };
    }

    let hue = ((libm::atan2(lab_b, lab_a) * 180.0) / std::f64::consts::PI + 360.0) % 360.0;
    let (c0, c_mid, c_max) = chroma_stops(l, lab_a / chroma, lab_b / chroma);
    let saturation = if chroma < c_mid {
        let k1 = 0.8 * c0;
        0.8 * (chroma / (k1 + (1.0 - k1 / c_mid) * chroma))
    } else {
        let k1 = (0.2 * c_mid.powf(2.0) * 1.25_f64.powf(2.0)) / c0;
        let offset = chroma - c_mid;
        0.8 + 0.2 * (offset / (k1 + (1.0 - k1 / (c_max - c_mid)) * offset))
    };
    OkhslChannels {
        h: hue,
        s: saturation.clamp(0.0, 1.0),
        l: lightness,
    }
}
