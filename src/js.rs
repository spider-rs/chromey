use serde::de::DeserializeOwned;

use chromey_cdp::cdp::js_protocol::runtime::{
    CallFunctionOnParams, EvaluateParams, RemoteObject,
};

use crate::utils::is_likely_js_function;

#[derive(Debug, Clone)]
pub struct EvaluationResult {
    /// Mirror object referencing original JavaScript object
    inner: RemoteObject,
}

impl EvaluationResult {
    /// Creates a new evaluation result from a remote object.
    pub fn new(inner: RemoteObject) -> Self {
        Self { inner }
    }
    /// Get a reference to the underlying remote object.
    pub fn object(&self) -> &RemoteObject {
        &self.inner
    }

    /// Get the deserialized value if available.
    pub fn value(&self) -> Option<&serde_json::Value> {
        self.object().value.as_ref()
    }

    /// Attempts to deserialize the value into the given type
    pub fn into_value<T: DeserializeOwned>(self) -> serde_json::Result<T> {
        let value = self
            .inner
            .value
            .ok_or_else(|| serde::de::Error::custom("No value found"))?;
        serde_json::from_value(value)
    }

    /// Attempts to deserialize the value into as bytes.
    pub fn into_bytes(self) -> serde_json::Result<Vec<u8>> {
        let value = self
            .inner
            .value
            .ok_or_else(|| serde::de::Error::custom("No value found"))?;

        Ok(value.as_str().unwrap_or_default().into())
    }
}

#[derive(Debug, Clone)]
pub enum Evaluation {
    /// A JavaScript expression to evaluate.
    Expression(EvaluateParams),
    /// A JavaScript function to invoke.
    Function(CallFunctionOnParams),
}

impl From<&str> for Evaluation {
    fn from(expression: &str) -> Self {
        if is_likely_js_function(expression) {
            CallFunctionOnParams::from(expression).into()
        } else {
            EvaluateParams::from(expression).into()
        }
    }
}

impl From<String> for Evaluation {
    fn from(expression: String) -> Self {
        expression.as_str().into()
    }
}

impl From<EvaluateParams> for Evaluation {
    fn from(params: EvaluateParams) -> Self {
        Evaluation::Expression(params)
    }
}

impl From<CallFunctionOnParams> for Evaluation {
    fn from(params: CallFunctionOnParams) -> Self {
        Evaluation::Function(params)
    }
}
