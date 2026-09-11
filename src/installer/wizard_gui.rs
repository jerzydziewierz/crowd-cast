//! GUI-based setup wizard
//!
//! On macOS, this uses native Cocoa UI via FFI.
//! On other platforms, a fallback implementation may be used.

use anyhow::Result;
use tracing::info;

use crate::capture::list_capturable_apps;
use crate::config::Config;
use crate::installer::autostart::{disable_autostart, enable_autostart, AutostartConfig};

#[cfg(any(target_os = "macos", target_os = "linux"))]
use super::wizard_ffi::{self, AppInfoWrapper};

/// Result of running the GUI wizard
#[derive(Debug, Clone)]
pub struct WizardResult {
    /// Whether setup completed successfully
    pub completed: bool,
    /// Selected applications for capture
    pub selected_apps: Vec<String>,
    /// Whether to capture all apps
    pub capture_all: bool,
    /// Whether autostart was enabled
    pub autostart_enabled: bool,
}

impl Default for WizardResult {
    fn default() -> Self {
        Self {
            completed: false,
            selected_apps: vec![],
            capture_all: false,
            autostart_enabled: false,
        }
    }
}

/// Run the GUI setup wizard
///
/// On macOS, this launches a native Cocoa window.
/// On other platforms, returns an error indicating native wizard is not available.
pub fn run_wizard_gui(config: &mut Config) -> Result<WizardResult> {
    info!("Starting native setup wizard");

    #[cfg(any(target_os = "macos", target_os = "linux"))]
    {
        run_wizard_native(config)
    }

    #[cfg(target_os = "windows")]
    {
        super::wizard_windows::run_wizard_windows(config)
    }

    #[cfg(not(any(target_os = "macos", target_os = "windows", target_os = "linux")))]
    {
        let _ = config;
        anyhow::bail!("Native GUI wizard is not available on this platform. Edit config.toml manually.");
    }
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
fn run_wizard_native(config: &mut Config) -> Result<WizardResult> {
    // Get list of available apps
    info!("Loading available applications...");
    let apps = list_capturable_apps();

    // Convert to FFI format
    let app_wrappers: Vec<AppInfoWrapper> = apps
        .iter()
        .map(|a| AppInfoWrapper::new(&a.bundle_id, &a.name, a.pid))
        .collect();

    // Set apps in the native wizard
    wizard_ffi::set_available_apps(&app_wrappers);

    // Seed the checklist with the selection already in the config. The wizard re-runs for
    // reasons that have nothing to do with first setup -- a revoked TCC grant, an unmet host
    // requirement, an autostart mismatch (see `main.rs`) -- and it saves the whole list on
    // Finish, so an unseeded checklist silently replaces the user's whitelist with whatever
    // happens to be ticked in that moment. Apps that aren't running are shown as pre-ticked
    // "(not running)" rows so the full whitelist is visible and removals stay deliberate.
    // Must come after `set_available_apps`, which clears the native selection.
    wizard_ffi::set_current_selection(&config.capture.target_apps, config.capture.capture_all);

    // Linux: detect host requirements (GPU, screen-capture backend, input group,
    // VAAPI) and hand them to the wizard to display + gate Finish on.
    #[cfg(target_os = "linux")]
    {
        let reqs = crate::installer::requirements::collect(config.capture.start_on_login);
        wizard_ffi::set_requirements(&reqs);
        wizard_ffi::set_per_app_available(
            crate::installer::requirements::per_app_capture_available(),
        );
    }

    // Run the native wizard (blocks until closed). Seed the autostart checkbox with the
    // saved preference so a re-opened wizard reflects the user's actual state rather than
    // silently defaulting it off.
    info!("Launching native wizard window...");
    let native_result = wizard_ffi::run_native_wizard(config.capture.start_on_login);

    // Convert result
    let result = WizardResult {
        completed: native_result.completed,
        selected_apps: native_result.selected_apps.clone(),
        capture_all: native_result.capture_all,
        autostart_enabled: native_result.enable_autostart,
    };

    // If wizard completed, update and save config
    if result.completed {
        info!("Wizard completed successfully");

        // Log what the save actually changed, so a regression in the seeding above shows up
        // in shipped logs without a fleet-wide sweep.
        let (added, removed) = selection_diff(&config.capture.target_apps, &result.selected_apps);
        info!(
            "Wizard app selection saved: capture_all={}, kept={}, added={:?}, removed={:?}",
            result.capture_all,
            result.selected_apps.len().saturating_sub(added.len()),
            added,
            removed
        );

        // Update config
        config.capture.capture_all = result.capture_all;
        config.capture.target_apps = result.selected_apps.clone();
        config.capture.setup_completed = true;
        config.capture.start_on_login = result.autostart_enabled;

        // Enable autostart if requested
        if result.autostart_enabled {
            let autostart_config = AutostartConfig::default();
            if let Err(e) = enable_autostart(&autostart_config) {
                info!("Failed to enable autostart: {}", e);
            } else {
                info!("Autostart enabled");
            }
        } else if let Err(e) = disable_autostart() {
            info!("Failed to disable autostart: {}", e);
        } else {
            info!("Autostart disabled");
        }

        // Save config
        config.save()?;
        info!("Configuration saved");
    } else {
        info!("Wizard was cancelled");
    }

    Ok(result)
}

/// What a wizard save changed: apps the user added, and apps that were in the saved
/// whitelist and are not in the new selection.
///
/// Compared by exact string, which is what both native pickers round-trip (macOS bundle IDs
/// come from the seed or from `NSRunningApplication`; the Linux GTK rows carry the saved id
/// verbatim), so a "removed" entry here is a real removal rather than a casing difference.
fn selection_diff<'a>(
    previous: &'a [String],
    selected: &'a [String],
) -> (Vec<&'a String>, Vec<&'a String>) {
    let added = selected.iter().filter(|a| !previous.contains(a)).collect();
    let removed = previous.iter().filter(|a| !selected.contains(a)).collect();
    (added, removed)
}

#[cfg(test)]
mod tests {
    use super::selection_diff;

    fn v(items: &[&str]) -> Vec<String> {
        items.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn unchanged_selection_reports_no_diff() {
        let previous = v(&["com.apple.Safari", "com.microsoft.VSCode"]);
        let selected = previous.clone();
        let (added, removed) = selection_diff(&previous, &selected);
        assert!(added.is_empty());
        assert!(removed.is_empty());
    }

    #[test]
    fn seeded_rerun_keeps_apps_that_were_not_running() {
        // The whole point of seeding: a re-run that the user clicks through without touching
        // the checklist returns the saved list, including the apps with "(not running)" rows.
        let previous = v(&["com.apple.Safari", "com.figma.Desktop"]);
        let selected = v(&["com.figma.Desktop", "com.apple.Safari"]); // order is not meaningful
        let (added, removed) = selection_diff(&previous, &selected);
        assert!(added.is_empty());
        assert!(removed.is_empty(), "a seeded re-run must not drop apps");
    }

    #[test]
    fn deliberate_removal_is_reported() {
        let previous = v(&["com.apple.Safari", "com.microsoft.VSCode"]);
        let selected = v(&["com.apple.Safari"]);
        let (added, removed) = selection_diff(&previous, &selected);
        assert!(added.is_empty());
        assert_eq!(removed, vec![&"com.microsoft.VSCode".to_string()]);
    }

    #[test]
    fn additions_and_removals_are_reported_together() {
        let previous = v(&["com.apple.Safari"]);
        let selected = v(&["com.microsoft.VSCode"]);
        let (added, removed) = selection_diff(&previous, &selected);
        assert_eq!(added, vec![&"com.microsoft.VSCode".to_string()]);
        assert_eq!(removed, vec![&"com.apple.Safari".to_string()]);
    }

    #[test]
    fn unseeded_wizard_wiping_the_list_is_visible_as_removals() {
        // The pre-fix behaviour, kept as a regression witness: an empty result against a
        // non-empty saved list must show up as every app removed, not as a silent no-op.
        let previous = v(&["com.apple.Safari", "com.microsoft.VSCode", "com.figma.Desktop"]);
        let (added, removed) = selection_diff(&previous, &[]);
        assert!(added.is_empty());
        assert_eq!(removed.len(), 3);
    }
}
