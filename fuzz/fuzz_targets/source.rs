#![no_main]

use std::collections::BTreeMap;

use herdr_plugin_manager_fuzz::{source, util};
use libfuzzer_sys::fuzz_target;

fuzz_target!(|text: &str| {
    let aliases = BTreeMap::from([(
        "corp".to_string(),
        "ssh://git@example.invalid/{path}.git".to_string(),
    )]);
    if let Ok(parsed) = source::parse(text, &aliases) {
        let source::Source::Git { subdir, .. } = parsed.into_git() else {
            unreachable!("all sources must use the Git pipeline");
        };
        if let Some(subdir) = subdir {
            assert!(!subdir.split('/').any(|part| part == ".."));
            assert_eq!(subdir, source::normalize_subdir(&subdir));
        }
    }
    let safe = util::sanitize(text);
    assert!(!safe.chars().any(char::is_control));
    assert_eq!(safe, util::sanitize(&safe));
});
