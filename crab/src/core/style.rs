//! Centralized color and styling for CLI output.
//!
//! All user-facing status indicators (✓, ⚠, ✗) route through [`CliStyle`]
//! so color decisions are resolved once per invocation and respected
//! consistently across commands.

use console::{Style, Term};

use super::output::OutputMode;

/// Resolved color configuration for the current invocation.
///
/// Created once at CLI entry and threaded to commands that emit
/// user-facing status indicators.
pub struct CliStyle {
    pub success: Style,
    pub warning: Style,
    pub error: Style,
    pub dim: Style,
    pub bold: Style,
    enabled: bool,
}

impl CliStyle {
    /// Resolve color support from terminal capabilities, `NO_COLOR` env,
    /// and [`OutputMode`].
    ///
    /// Color is enabled only when all three conditions hold:
    /// 1. OutputMode is Text (machine modes never get ANSI codes).
    /// 2. The `NO_COLOR` environment variable is not set.
    /// 3. The terminal reports color support.
    pub fn resolve(mode: OutputMode) -> Self {
        let enabled = mode == OutputMode::Text
            && std::env::var_os("NO_COLOR").is_none()
            && Term::stderr().features().colors_supported();

        if enabled {
            Self {
                success: Style::new().green(),
                warning: Style::new().yellow(),
                error: Style::new().red(),
                dim: Style::new().dim(),
                bold: Style::new().bold(),
                enabled: true,
            }
        } else {
            Self {
                success: Style::new(),
                warning: Style::new(),
                error: Style::new(),
                dim: Style::new(),
                bold: Style::new(),
                enabled: false,
            }
        }
    }

    /// Whether color output is active.
    pub fn is_enabled(&self) -> bool {
        self.enabled
    }

    /// Format a success indicator: "✓ message" in green, or "OK: message" when
    /// color is disabled.
    pub fn ok(&self, msg: &str) -> String {
        if self.enabled {
            format!("{}", self.success.apply_to(format!("✓ {msg}")))
        } else {
            format!("OK: {msg}")
        }
    }

    /// Format a warning indicator: "⚠ message" in yellow, or "WARN: message"
    /// when color is disabled.
    pub fn warn(&self, msg: &str) -> String {
        if self.enabled {
            format!("{}", self.warning.apply_to(format!("⚠ {msg}")))
        } else {
            format!("WARN: {msg}")
        }
    }

    /// Format an error indicator: "✗ message" in red, or "ERROR: message"
    /// when color is disabled.
    pub fn err(&self, msg: &str) -> String {
        if self.enabled {
            format!("{}", self.error.apply_to(format!("✗ {msg}")))
        } else {
            format!("ERROR: {msg}")
        }
    }

    /// Format dimmed/secondary text for less-important output.
    pub fn dim(&self, msg: &str) -> String {
        if self.enabled {
            format!("{}", self.dim.apply_to(msg))
        } else {
            msg.to_string()
        }
    }

    /// Format bold/emphasized text for headings.
    pub fn bold(&self, msg: &str) -> String {
        if self.enabled {
            format!("{}", self.bold.apply_to(msg))
        } else {
            msg.to_string()
        }
    }
}
