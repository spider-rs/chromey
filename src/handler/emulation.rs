use chromey_cdp::cdp::browser_protocol::emulation::{
    ScreenOrientation, ScreenOrientationType, SetDeviceMetricsOverrideParams,
    SetTouchEmulationEnabledParams,
};
use chromey_types::Method;

use crate::cmd::CommandChain;
use crate::handler::viewport::Viewport;
use std::time::Duration;

#[derive(Default, Debug, Clone, Copy, PartialEq)]
/// Emulation manager.
pub struct EmulationManager {
    /// Whether mobile emulation is enabled.
    pub emulating_mobile: bool,
    /// Whether touch input is enabled.
    pub has_touch: bool,
    /// Whether a reload is required to apply new emulation settings.
    pub needs_reload: bool,
    /// Timeout to apply emulation requests.
    pub request_timeout: Duration,
}

impl EmulationManager {
    /// Creates a new `EmulationManager` with the given request timeout.
    pub fn new(request_timeout: Duration) -> Self {
        Self {
            emulating_mobile: false,
            has_touch: false,
            needs_reload: false,
            request_timeout,
        }
    }
    /// Generates the initial emulation commands based on the provided viewport.
    ///
    /// This sets up device metrics and touch emulation, and updates internal flags
    /// to determine if a page reload is required to apply the changes.
    pub fn init_commands(&mut self, viewport: &Viewport) -> CommandChain {
        let mut chains = Vec::with_capacity(2);
        let set_touch = SetTouchEmulationEnabledParams::new(viewport.emulating_mobile);
        let orientation = if viewport.is_landscape {
            ScreenOrientation::new(ScreenOrientationType::LandscapePrimary, 90)
        } else {
            ScreenOrientation::new(ScreenOrientationType::PortraitPrimary, 0)
        };

        if let Ok(set_device) = SetDeviceMetricsOverrideParams::builder()
            .mobile(viewport.emulating_mobile)
            .width(viewport.width)
            .height(viewport.height)
            .device_scale_factor(viewport.device_scale_factor.unwrap_or(1.))
            .screen_orientation(orientation)
            .build()
        {
            if let Ok(set_device_value) = serde_json::to_value(&set_device) {
                chains.push((set_device.identifier(), set_device_value));
            }
        }

        if let Ok(set_touch_value) = serde_json::to_value(&set_touch) {
            chains.push((set_touch.identifier(), set_touch_value));
        }

        let chain = CommandChain::new(chains, self.request_timeout);

        self.needs_reload = self.emulating_mobile != viewport.emulating_mobile
            || self.has_touch != viewport.has_touch;
        chain
    }
}
