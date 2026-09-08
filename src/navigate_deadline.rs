//! Client binding for `Page.navigate` carrying a server-side navigation
//! deadline.
//!
//! Some CDP servers accept an extra `timeout` key on `Page.navigate` params and
//! abandon the navigation themselves once it elapses, so the caller gets a real
//! navigate ack instead of a client-side abort with no protocol trace. The key
//! is not part of the DevTools protocol, and stock Chrome ignores unknown
//! params, so sending it is inert against an engine that does not implement it.
//!
//! Like the other vendor bindings in this crate (see
//! [`crate::content_markdown`], or how `WebMCP.listTools` is sent), this is a
//! hand-written [`Command`] implementation issued under a raw method string.
//! Nothing is added to the PDL and nothing is regenerated, so
//! [`NavigateParams`] keeps the exact shape and wire format it has always had
//! and every existing caller is untouched.

use std::time::Duration;

use chromiumoxide_cdp::cdp::browser_protocol::page::{NavigateParams, NavigateReturns};
use serde::Serialize;

/// Standard `Page.navigate` params plus a `timeout` deadline in milliseconds.
///
/// Serializes as the flattened [`NavigateParams`] object with one extra
/// `timeout` key, and is sent under the standard `Page.navigate` method string.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct NavigateWithDeadlineParams {
    #[serde(flatten)]
    inner: NavigateParams,
    /// Navigation deadline in milliseconds.
    timeout: i64,
}

impl NavigateWithDeadlineParams {
    /// Arm `params` with a navigation deadline.
    ///
    /// The deadline is sent as whole milliseconds. Durations too large for an
    /// `i64` saturate at [`i64::MAX`] rather than wrapping, and the value is
    /// never negative.
    pub fn new(params: impl Into<NavigateParams>, navigation_timeout: Duration) -> Self {
        Self {
            inner: params.into(),
            timeout: duration_as_millis_i64(navigation_timeout),
        }
    }

    /// The standard navigate params this deadline is attached to.
    pub fn params(&self) -> &NavigateParams {
        &self.inner
    }

    /// The URL being navigated to.
    pub fn url(&self) -> &str {
        &self.inner.url
    }

    /// The deadline in milliseconds, as it goes on the wire.
    pub fn timeout_millis(&self) -> i64 {
        self.timeout
    }

    /// Drop the deadline and return the standard params.
    pub fn into_params(self) -> NavigateParams {
        self.inner
    }
}

/// Whole milliseconds of `duration`, saturating at [`i64::MAX`].
#[inline]
fn duration_as_millis_i64(duration: Duration) -> i64 {
    i64::try_from(duration.as_millis()).unwrap_or(i64::MAX)
}

impl chromiumoxide_types::Method for NavigateWithDeadlineParams {
    fn identifier(&self) -> chromiumoxide_types::MethodId {
        NavigateParams::IDENTIFIER.into()
    }
}

impl chromiumoxide_types::MethodType for NavigateWithDeadlineParams {
    fn method_id() -> chromiumoxide_types::MethodId
    where
        Self: Sized,
    {
        NavigateParams::IDENTIFIER.into()
    }
}

impl chromiumoxide_types::Command for NavigateWithDeadlineParams {
    type Response = NavigateReturns;
}

#[cfg(test)]
mod tests {
    use super::*;
    use chromiumoxide_types::{Method, MethodType};

    #[test]
    fn method_string_is_page_navigate() {
        let params = NavigateWithDeadlineParams::new(
            NavigateParams::new("https://example.test/"),
            Duration::from_secs(3),
        );
        assert_eq!(params.identifier().as_ref(), "Page.navigate");
        assert_eq!(
            NavigateWithDeadlineParams::method_id().as_ref(),
            NavigateParams::IDENTIFIER
        );
    }

    #[test]
    fn timeout_is_integer_milliseconds() {
        let params = NavigateWithDeadlineParams::new(
            NavigateParams::new("https://example.test/"),
            Duration::from_secs(3),
        );
        let value = serde_json::to_value(&params).expect("serialize");
        assert_eq!(value["timeout"], serde_json::json!(3000));
        assert_eq!(params.timeout_millis(), 3000);
    }

    #[test]
    fn oversized_duration_saturates_instead_of_wrapping() {
        let params = NavigateWithDeadlineParams::new(
            NavigateParams::new("https://example.test/"),
            Duration::from_secs(u64::MAX),
        );
        assert_eq!(params.timeout_millis(), i64::MAX);
        let value = serde_json::to_value(&params).expect("serialize");
        assert_eq!(value["timeout"], serde_json::json!(i64::MAX));
        assert!(
            value["timeout"].as_i64().is_some_and(|ms| ms > 0),
            "deadline must never serialize as a negative value"
        );
    }

    #[test]
    fn zero_duration_is_zero_millis() {
        let params = NavigateWithDeadlineParams::new(
            NavigateParams::new("https://example.test/"),
            Duration::ZERO,
        );
        assert_eq!(params.timeout_millis(), 0);
    }

    #[test]
    fn flatten_keeps_every_standard_field_identical() {
        let base = NavigateParams {
            url: "https://example.test/page".into(),
            referrer: Some("https://referrer.test/".into()),
            transition_type: Some(
                chromiumoxide_cdp::cdp::browser_protocol::page::TransitionType::Link,
            ),
            frame_id: Some(
                chromiumoxide_cdp::cdp::browser_protocol::page::FrameId::from(
                    "frame-abc".to_string(),
                ),
            ),
            referrer_policy: Some(
                chromiumoxide_cdp::cdp::browser_protocol::page::ReferrerPolicy::NoReferrer,
            ),
        };
        let plain = serde_json::to_value(&base).expect("serialize plain");
        let armed = serde_json::to_value(NavigateWithDeadlineParams::new(
            base,
            Duration::from_millis(1500),
        ))
        .expect("serialize armed");

        let plain = plain.as_object().expect("plain object");
        let armed_obj = armed.as_object().expect("armed object");

        for (key, value) in plain {
            assert_eq!(
                armed_obj.get(key),
                Some(value),
                "key `{key}` diverged on the armed path"
            );
        }
        let mut extra: Vec<&String> = armed_obj
            .keys()
            .filter(|k| !plain.contains_key(*k))
            .collect();
        extra.sort();
        assert_eq!(extra, vec![&"timeout".to_string()]);
    }

    #[test]
    fn plain_params_carry_no_timeout_key() {
        let value =
            serde_json::to_value(NavigateParams::new("https://example.test/")).expect("serialize");
        let object = value.as_object().expect("object");
        assert!(!object.contains_key("timeout"));
        assert_eq!(object.keys().collect::<Vec<_>>(), vec!["url"]);
    }
}
