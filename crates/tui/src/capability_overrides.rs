//! terminal-image.ts `getCapabilities` under `setCapabilityOverrides` (the `terminal.trueColor`,
//! `terminal.hyperlinks` and `terminal.images` settings): the detected capabilities with each configured one forced.
//! A configured `hyperlinks` also stands in for the tmux probe, so a forced value is never second-guessed by it.

use crate::image::{Capabilities, ImageProtocol, detect_capabilities};

/// What the settings force; `None` keeps the detected value. `images: Some(None)` turns inline images off.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct CapabilityOverrides {
    pub images: Option<Option<ImageProtocol>>,
    pub true_color: Option<bool>,
    pub hyperlinks: Option<bool>,
}

/// `getCapabilities()` after `setCapabilityOverrides(overrides)`: [`detect_capabilities`] (environment and the
/// `PI_*` variables) with `tmux_forwards_hyperlinks` as the tmux probe's answer, then the overrides on top.
pub fn capabilities(
    env: &dyn Fn(&str) -> Option<String>,
    windows: bool,
    tmux_forwards_hyperlinks: bool,
    overrides: &CapabilityOverrides,
) -> Capabilities {
    let mut caps = detect_capabilities(env, windows, overrides.hyperlinks.unwrap_or(tmux_forwards_hyperlinks));
    if let Some(images) = overrides.images {
        caps.images = images;
    }
    if let Some(true_color) = overrides.true_color {
        caps.true_color = true_color;
    }
    if let Some(hyperlinks) = overrides.hyperlinks {
        caps.hyperlinks = hyperlinks;
    }
    caps
}
