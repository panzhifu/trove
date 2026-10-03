//! Diagnostic: what does cpal see?
//!
//! Run with the headphones disconnected, then connected, and compare:
//!
//! ```sh
//! cargo run -p trove-app --example audio_probe
//! ```
//!
//! The app opens the best of the dynamic-routing PCMs ("pulse", then
//! "pipewire") so the sound server itself routes the stream to whatever
//! the default output is, and falls back to the bare "default" alias. It
//! also fingerprints the device set on a two second clock, because an
//! ALSA host reports the default as an alias whose name never moves.

use rodio::cpal::traits::{DeviceTrait as _, HostTrait as _};

fn main() {
    let host = rodio::cpal::default_host();
    println!("host: {}", host.id().name());
    let mut names: Vec<String> = match host.output_devices() {
        Ok(devices) => devices.filter_map(|d| d.name().ok()).collect(),
        Err(error) => {
            println!("output devices: enumeration failed ({error})");
            return;
        }
    };
    names.sort();
    println!("output devices ({}):", names.len());
    for name in &names {
        println!("  - {name}");
    }
    match host.default_output_device() {
        Some(device) => match device.name() {
            Ok(name) => println!("default output: {name}"),
            Err(error) => println!("default output: name unavailable ({error})"),
        },
        None => println!("default output: none"),
    }
    let chosen = ["pulse", "pipewire"]
        .into_iter()
        .find(|alias| names.iter().any(|n| n == alias))
        .unwrap_or("default");
    println!("trove will open: {chosen}");
}
