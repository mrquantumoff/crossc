//! Generates the PipeWire codec-plugin ABI from the private bluez5 headers
//! vendored for the PipeWire version this plugin targets, and picks the codec
//! slot the plugin registers in.

use std::env;
use std::path::PathBuf;
use std::process::Command;

const VENDORED_PIPEWIRE: &str = "1.6.11";

fn pkg_config(args: &[&str]) -> Option<String> {
    let out = Command::new("pkg-config").args(args).output().ok()?;
    out.status
        .success()
        .then(|| String::from_utf8_lossy(&out.stdout).trim().to_owned())
}

fn main() {
    let manifest = PathBuf::from(env::var("CARGO_MANIFEST_DIR").unwrap());
    let headers = env::var_os("SSC_PIPEWIRE_HEADERS")
        .map(PathBuf::from)
        .unwrap_or_else(|| manifest.join("../pipewire").join(VENDORED_PIPEWIRE));
    println!("cargo:rerun-if-env-changed=SSC_PIPEWIRE_HEADERS");
    println!("cargo:rerun-if-env-changed=SSC_CODEC_SLOT");
    println!("cargo:rerun-if-env-changed=SSC_SPA_INCLUDE");
    println!("cargo:rerun-if-changed=wrapper.h");
    println!("cargo:rerun-if-changed={}", headers.display());

    if env::var_os("SSC_PIPEWIRE_HEADERS").is_none() {
        match pkg_config(&["--modversion", "libpipewire-0.3"]) {
            Some(v) if v == VENDORED_PIPEWIRE => {}
            Some(v) => println!(
                "cargo:warning=installed PipeWire is {v}, the vendored codec ABI is {VENDORED_PIPEWIRE}; \
                 set SSC_PIPEWIRE_HEADERS to spa/plugins/bluez5 of a PipeWire {v} source tree"
            ),
            None => println!(
                "cargo:warning=pkg-config does not know libpipewire-0.3; assuming {VENDORED_PIPEWIRE}"
            ),
        }
    }

    // Release builds point this at spa/include of the PipeWire source the vendored headers come from.
    let spa_cflags = match env::var("SSC_SPA_INCLUDE") {
        Ok(dir) => format!("-I{dir}"),
        Err(_) => pkg_config(&["--cflags", "libspa-0.2"])
            .unwrap_or_else(|| "-I/usr/include/spa-0.2".into()),
    };
    let bindings = bindgen::Builder::default()
        .header(manifest.join("wrapper.h").to_string_lossy())
        .clang_arg(format!("-I{}", headers.display()))
        .clang_args(spa_cflags.split_whitespace())
        .allowlist_type("media_codec|spa_bluez5_codec_a2dp|spa_handle|spa_handle_factory|spa_interface_info")
        .allowlist_type("spa_support|spa_dict|spa_dict_item|spa_log|spa_log_methods|spa_log_topic|spa_log_topic_enum")
        .allowlist_type("spa_pod_builder|spa_pod_builder_callbacks|spa_pod_frame|spa_audio_info")
        .allowlist_type("spa_bluetooth_audio_codec|spa_audio_format|spa_audio_channel|spa_log_level")
        .allowlist_type("spa_format|spa_media_type|spa_media_subtype|spa_choice_type")
        .allowlist_var("SPA_VERSION_.*|A2DP_CODEC_VENDOR|A2DP_MAX_CAPS_SIZE|A2DP_CODEC_DEFAULT_RATE|NEED_FLUSH_.*|MEDIA_CODEC_FLAG_SINK")
        .allowlist_var("SPA_TYPE_.*|SPA_FORMAT_.*|SPA_MEDIA_TYPE_audio|SPA_MEDIA_SUBTYPE_raw|SPA_CHOICE_Enum")
        .allowlist_var("SPA_LOG_LEVEL_.*|SPA_AUDIO_FORMAT_.*|SPA_AUDIO_CHANNEL_F[LR]|SPA_BLUETOOTH_AUDIO_CODEC_.*")
        .allowlist_var("MEDIA_CODEC_A2DP")
        .default_enum_style(bindgen::EnumVariation::Consts)
        .prepend_enum_name(false)
        .derive_default(true)
        .generate_cstr(true)
        .generate()
        .expect("bindgen failed on the PipeWire codec headers");

    let out = PathBuf::from(env::var("OUT_DIR").unwrap());
    let text = bindings.to_string();
    std::fs::write(out.join("bindings.rs"), &text).unwrap();

    // A PipeWire carrying patches/pipewire-ssc.patch has its own SSC codec id
    // and loader slot. Unpatched, the plugin takes the LC3plus slot, which
    // distributions leave empty (liblc3plus is not packaged).
    let has_ssc_id = text.contains("SPA_BLUETOOTH_AUDIO_CODEC_SSC:");
    let slot = env::var("SSC_CODEC_SLOT")
        .unwrap_or_else(|_| if has_ssc_id { "ssc" } else { "lc3plus" }.into());
    let id = match slot.as_str() {
        "ssc" if has_ssc_id => "SPA_BLUETOOTH_AUDIO_CODEC_SSC",
        "ssc" => panic!(
            "SSC_CODEC_SLOT=ssc needs PipeWire headers from a build with patches/pipewire-ssc.patch"
        ),
        "lc3plus" => "SPA_BLUETOOTH_AUDIO_CODEC_LC3PLUS_HR",
        other => panic!("SSC_CODEC_SLOT={other}: expected ssc or lc3plus"),
    };
    std::fs::write(
        out.join("slot.rs"),
        format!(
            "pub const CODEC_ENUM: crate::sys::spa_bluetooth_audio_codec = crate::sys::{id};\n\
             pub const FACTORY_NAME: &::core::ffi::CStr = c\"api.codec.bluez5.media.{slot}\";\n\
             pub const LOG_TOPIC: &::core::ffi::CStr = c\"spa.bluez5.codecs.{slot}\";\n\
             pub const SLOT: &str = \"{slot}\";\n"
        ),
    )
    .unwrap();
}
