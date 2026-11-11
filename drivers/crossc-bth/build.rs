//! Build script: standard windows-drivers-rs binary configuration, plus bindgen
//! bindings for the Bluetooth profile-driver DDI (bthdef.h / bthsdpdef.h /
//! bthioctl.h / bthddi.h), which wdk-sys 0.5 does not cover.
//!
//! Only *items declared in the bth headers* are emitted
//! (`allowlist_recursively(false)`); everything they reference (LIST_ENTRY,
//! NTSTATUS, INTERFACE, ...) resolves to the wdk-sys definitions via a glob
//! import in `src/bth.rs`, so the types are identical to the ones WDF uses.
//! bindgen emits compile-time size/align/offset assertions for every struct
//! (computed by clang from the real headers), so a layout mismatch fails the
//! build.
//!
//! Function-like macros (CTL_CODE-based IOCTLs) and DEFINE_GUID GUIDs are not
//! expressible by bindgen; they are hand-written in `src/bth.rs` and checked
//! against cl.exe output (see `tools/bthcheck/`).
//!
//! A second pass does the same for ACX 1.1 (`acx.h` and the few KS / MM types
//! it references), written to `$OUT_DIR/acx.rs` for `src/acx.rs`, and links
//! `acxstub.lib` (the class-extension bind info and function table; it holds
//! data only, no code). See `src/acx.rs` and `tools/acxcheck/`.

use std::path::PathBuf;

use wdk_build::{ApiSubset, BuilderExt, Config};

const BTH_HEADERS: &[&str] = &["bthdef.h", "bthsdpdef.h", "bthioctl.h", "bthddi.h"];

/// Generic A2DP-sink IDs that must never appear on a model line: with a
/// trusted test cert they tie with the inbox BthA2dp INF on signature and
/// identifier score and win on DriverVer date, i.e. crossc_bth would take
/// over every A2DP headset on its next (re)install.
const FORBIDDEN_INF_IDS: &[&str] =
    &[r"BTHENUM\{0000110b-0000-1000-8000-00805f9b34fb}", r"BTHENUM\{0000110b-0000-1000-8000-00805f9b34fb}_GENERIC"];

/// Fails the build if any non-comment line of the INX names a forbidden ID.
fn check_inx_model_lines() {
    let inx = std::fs::read_to_string("crossc_bth.inx").expect("read crossc_bth.inx");
    for (n, line) in inx.lines().enumerate() {
        let code = line.split(';').next().unwrap_or("");
        for id in code.split([',', '=']).map(str::trim) {
            if FORBIDDEN_INF_IDS.iter().any(|f| f.eq_ignore_ascii_case(id)) {
                panic!(
                    "crossc_bth.inx:{}: model line uses the generic A2DP ID {id}. It would outrank BthA2dp for EVERY headset once the test cert is trusted. Use the buds-specific hardware ID only.",
                    n + 1
                );
            }
        }
    }
    println!("cargo::rerun-if-changed=crossc_bth.inx");
}

fn main() -> Result<(), wdk_build::ConfigError> {
    check_inx_model_lines();
    wdk_build::configure_wdk_binary_build()?;

    let config = Config::from_env_auto()?;
    let mut header = config.bindgen_header_contents([ApiSubset::Base, ApiSubset::Wdf])?;
    for h in BTH_HEADERS {
        header.push_str(&format!("#include <{h}>\n"));
    }

    let mut builder = bindgen::Builder::wdk_default(&config)?
        .header_contents("crossc-bth-input.h", &header)
        .allowlist_recursively(false);
    for h in BTH_HEADERS {
        let stem = h.trim_end_matches(".h");
        builder = builder.allowlist_file(format!(r"(?i).*[\\/]{stem}\.h"));
    }

    let out = PathBuf::from(std::env::var("OUT_DIR").expect("OUT_DIR"));
    builder
        .generate()
        .expect("bindgen failed for bth headers")
        .write_to_file(out.join("bth.rs"))
        .expect("write bth.rs");
    generate_acx(&config, &out)?;
    println!("cargo::rerun-if-changed=build.rs");
    Ok(())
}

/// ACX version this driver is built against. 1.1 is the only version
/// registered on current Windows (`Control\Wdf\Kmdf\Acx\Versions\1\1`).
/// `src/acx.rs` exports `AcxMinimumVersionRequired` with the same minor.
const ACX_VERSION: (u32, u32) = (1, 1);

fn generate_acx(config: &Config, out: &std::path::Path) -> Result<(), wdk_build::ConfigError> {
    let (major, minor) = ACX_VERSION;
    let ver = format!("{major}.{minor}");
    // <kit>/Include/<ver>/km -> km/acx/km/1.1, plus the sibling um dir
    // (mmsystem.h / mmreg.h, which define WAVEFORMATEX, live there).
    let km = config
        .include_paths()?
        .find(|p| p.file_name().is_some_and(|n| n.eq_ignore_ascii_case("km")))
        .expect("WDK km include dir");
    let acx_inc = km.join("acx").join("km").join(&ver);
    let um_inc = km.parent().expect("kit include root").join("um");
    assert!(acx_inc.join("acx.h").is_file(), "ACX headers not found at {}", acx_inc.display());

    // <kit>/Lib/<ver>/km/x64 -> acx/km/1.1/acxstub.lib
    let stub = config
        .library_paths()?
        .map(|p| p.join("acx").join("km").join(&ver).join("acxstub.lib"))
        .find(|p| p.is_file())
        .expect("acxstub.lib not found under the WDK km library dir");
    // Full path: the lib has no code, only the function table, the globals
    // and `_Acx_BIND_INFO` in section `.kmdfclassbind$b`. /INCLUDE keeps the
    // bind info even when nothing references it (stub.obj carries the same
    // directive, but only once the object is pulled in).
    // stub.obj also carries a selectany `WdfMinimumVersionRequired` (KMDF
    // minor 31). Resolving that symbol *before* the stub lib is on the command
    // line makes the linker take the wdk-sys copy (minor 33, as before ACX).
    // tools/acxcheck/check-image.ps1 verifies the result in the map file.
    println!("cargo::rustc-cdylib-link-arg=/INCLUDE:WdfMinimumVersionRequired");
    println!("cargo::rustc-cdylib-link-arg={}", stub.display());
    println!("cargo::rustc-cdylib-link-arg=/INCLUDE:_Acx_BIND_INFO");

    let mut header = config.bindgen_header_contents([ApiSubset::Base, ApiSubset::Wdf])?;
    // Same order as the Microsoft ACX samples' private.h.
    for h in ["windef.h", "ks.h", "mmsystem.h", "ksmedia.h", "acx.h"] {
        header.push_str(&format!("#include <{h}>\n"));
    }

    let builder = bindgen::Builder::wdk_default(config)?
        .header_contents("crossc-acx-input.h", &header)
        .clang_arg(format!("--include-directory={}", acx_inc.display()))
        .clang_arg(format!("--include-directory={}", um_inc.display()))
        .clang_arg(format!("--define-macro=ACX_VERSION_MAJOR={major}"))
        .clang_arg(format!("--define-macro=ACX_VERSION_MINOR={minor}"))
        .allowlist_recursively(false)
        .allowlist_file(r"(?i).*[\\/]acx[\\/]km[\\/].*[\\/]acx\w*\.h");
    builder
        .generate()
        .expect("bindgen failed for acx headers")
        .write_to_file(out.join("acx.rs"))
        .expect("write acx.rs");
    Ok(())
}
