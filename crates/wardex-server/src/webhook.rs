//! Webhook dispatcher (milestone M6).

use wardex_core::Config;
use wardex_core::manager::Manager;

/// Start delivering notable events to the configured webhooks.
pub fn spawn(_config: &Config, _manager: &Manager) {}
