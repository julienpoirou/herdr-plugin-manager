#![no_main]

use herdr_plugin_manager_fuzz::manifest::Manifest;
use libfuzzer_sys::fuzz_target;

fuzz_target!(|text: &str| {
    let Ok(manifest) = Manifest::parse(text) else {
        return;
    };
    // A round trip must preserve every command that will be reviewed/executed.
    let encoded = toml::to_string(&manifest).expect("manifest serialization");
    let decoded = Manifest::parse(&encoded).expect("serialized manifest parses");
    assert_eq!(manifest, decoded);
    if manifest.validate(None).is_ok() {
        // Validated, attacker-controlled metadata must not inject terminal controls.
        for line in manifest.preview_lines() {
            assert!(!line.chars().any(char::is_control));
        }
    }
});
