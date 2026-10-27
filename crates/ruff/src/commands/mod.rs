pub(crate) mod completions;

pub(crate) mod add_noqa;
pub(crate) mod analyze_graph;
pub(crate) mod check;
pub(crate) mod check_stdin;
pub(crate) mod clean;
pub(crate) mod config;
pub(crate) mod format;
pub(crate) mod format_stdin;
pub(crate) mod linter;
pub(crate) mod rule;
pub(crate) mod server;
pub(crate) mod show_files;
pub(crate) mod show_settings;
pub(crate) mod version;

use anyhow::{Result, anyhow};
use ruff_workspace::resolver::Resolver;

/// Fails if the plugins of any of the configurations that apply failed to load. Settings keep
/// these errors (see [`ruff_plugin::PluginSet::error`]), because only linting needs plugins.
pub(crate) fn check_plugins_loaded(resolver: &Resolver) -> Result<()> {
    match resolver
        .settings()
        .find_map(|settings| settings.linter.plugins.error())
    {
        Some(error) => Err(anyhow!("{error}")),
        None => Ok(()),
    }
}
