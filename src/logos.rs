//! Project or owner pictures for the popup, drawn only where the terminal
//! has an image protocol (Kitty, Sixel, iTerm2).

use std::path::PathBuf;
use std::time::Duration;

use anyhow::{Context, Result};

use crate::market::{self, Entry};
use crate::ops::{Origin, Row};
use crate::{paths, source, util};

/// Downloaded pictures are reused for a week.
const MAX_AGE: Duration = Duration::from_secs(7 * 86_400);
const MAX_BYTES: u64 = 2 * 1024 * 1024;

/// The picture of a marketplace entry: the one its source lists, or the
/// GitHub owner's.
pub fn for_entry(entry: &Entry) -> Option<String> {
    entry.image.clone().or_else(|| {
        entry
            .is_github()
            .then(|| entry.install.split('/').next().map(market::github_avatar))
            .flatten()
    })
}

/// The picture of an installed plugin from GitHub (Herdr's installer or a
/// GitHub remote in the git pipeline).
pub fn for_row(row: &Row) -> Option<String> {
    let owner = match row.origin {
        Origin::Github => row.plugin.source.github_slug()?,
        Origin::Git => {
            let record = row.record.as_ref()?;
            source::github_label(&record.url, None)?
        }
        Origin::Local => return None,
    };
    owner.split('/').next().map(market::github_avatar)
}

fn cache_file(url: &str) -> PathBuf {
    paths::market_cache_dir()
        .join("images")
        .join(util::fnv1a64(url))
}

/// The picture at `url`, from the disk cache when it is recent.
pub fn load(url: &str) -> Result<image::DynamicImage> {
    let file = cache_file(url);
    let fresh = std::fs::metadata(&file)
        .and_then(|m| m.modified())
        .is_ok_and(|t| t.elapsed().is_ok_and(|age| age < MAX_AGE));
    let bytes = if fresh {
        std::fs::read(&file)?
    } else {
        let bytes = market::http_get_bytes(url, Duration::from_secs(10), MAX_BYTES)?;
        if let Some(parent) = file.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let _ = std::fs::write(&file, &bytes);
        bytes
    };
    image::load_from_memory(&bytes).with_context(|| format!("{url} is not a picture"))
}
