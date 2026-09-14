//! Build: compile the icon gresource + generate the ensō SVGs (the
//! procedural brushstroke ported verbatim from src/render/svg.ts).

use std::env;
use std::fs;
use std::path::PathBuf;
use std::process::Command;

fn main() {
    println!("cargo:rerun-if-changed=assets/icons");

    let out = PathBuf::from(env::var("OUT_DIR").unwrap());

    // --- icons gresource ------------------------------------------------------
    let xml = PathBuf::from("assets/icons/satori.gresource.xml");
    let target = out.join("satori-icons.gresource");
    let status = Command::new("glib-compile-resources")
        .args([
            "--sourcedir",
            "assets/icons",
            "--target",
            target.to_str().unwrap(),
            "--generate",
            xml.to_str().unwrap(),
        ])
        .status()
        .expect("glib-compile-resources (glib2-devel)");
    assert!(status.success(), "glib-compile-resources failed");

    // --- ensō -------------------------------------------------------------------
    let light = enso_svg("#191714");
    let dark = enso_svg("#e9e3d6");
    fs::write(out.join("enso-light.svg"), light).unwrap();
    fs::write(out.join("enso-dark.svg"), dark).unwrap();
}

/// The brushstroke ensō: same constants as buildEnsoPath() — cx/cy 50, R 35,
/// 284° start, 340° sweep, wMax 18, sine-modulated radius and width with a
/// taper — plus the turbulence brush filter and dry-brush dash mask.
fn enso_svg(ink: &str) -> String {
    let (cx, cy, big_r) = (50.0_f64, 50.0_f64, 35.0_f64);
    let a0 = 284.0_f64.to_radians();
    let sweep = 340.0_f64.to_radians();
    let w_max = 18.0_f64;
    let n = 60;

    let mut outer = Vec::new();
    let mut inner = Vec::new();
    for i in 0..=n {
        let t = i as f64 / n as f64;
        let ang = a0 + sweep * t;
        let r = big_r + 2.4 * (ang * 3.0 + 0.6).sin();
        let taper = (std::f64::consts::PI * t).sin().powf(0.6);
        let w = w_max * taper * (0.9 + 0.15 * (ang * 5.0).sin());
        let (ro, ri) = (r + w / 2.0, r - w / 2.0);
        outer.push(format!(
            "{:.2} {:.2}",
            cx + ro * ang.cos(),
            cy + ro * ang.sin()
        ));
        inner.push(format!(
            "{:.2} {:.2}",
            cx + ri * ang.cos(),
            cy + ri * ang.sin()
        ));
    }
    inner.reverse();
    let d = format!("M {} L {} L {} Z", outer[0], outer[1..].join(" L "), inner.join(" L "));

    format!(
        r##"<?xml version="1.0" encoding="UTF-8"?>
<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 100 100" width="100" height="100">
  <defs>
    <filter id="enso-brush" x="-25%" y="-25%" width="150%" height="150%" color-interpolation-filters="sRGB">
      <feTurbulence type="fractalNoise" baseFrequency="0.045 0.06" numOctaves="3" seed="6" result="n"/>
      <feDisplacementMap in="SourceGraphic" in2="n" scale="6.5" xChannelSelector="R" yChannelSelector="G"/>
    </filter>
    <mask id="enso-dry" maskUnits="userSpaceOnUse" x="0" y="0" width="100" height="100">
      <rect width="100" height="100" fill="#fff"/>
      <g fill="none" stroke="#000" stroke-linecap="round">
        <circle cx="50" cy="50" r="43.5" stroke-width="1.5" stroke-dasharray="22 12 9 30" transform="rotate(8 50 50)"/>
        <circle cx="50" cy="50" r="39" stroke-width="1.1" stroke-dasharray="14 9 28 16" transform="rotate(64 50 50)"/>
        <circle cx="50" cy="50" r="35" stroke-width="1.4" stroke-dasharray="30 14 10 22" transform="rotate(146 50 50)"/>
        <circle cx="50" cy="50" r="31" stroke-width="1.0" stroke-dasharray="12 10 24 14" transform="rotate(214 50 50)"/>
        <circle cx="50" cy="50" r="27.5" stroke-width="1.3" stroke-dasharray="20 16 8 26" transform="rotate(298 50 50)"/>
      </g>
    </mask>
  </defs>
  <path d="{d}" fill="{ink}" filter="url(#enso-brush)" mask="url(#enso-dry)"/>
</svg>
"##
    )
}
