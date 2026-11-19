use serde_json::Value;

/// Galat PayPal. `Display` = `error.message` axios
/// (`Request failed with status code 422`, `timeout of 15000ms exceeded`, ...).
#[derive(Debug, Clone)]
pub struct PaypalError {
    pub message: String,
    /// `error.response.status` (bila server membalas).
    pub status: Option<u16>,
    /// `error.response.data` (bila server membalas; JSON, atau string bila bukan JSON).
    pub data: Option<Value>,
}

impl PaypalError {
    pub(crate) fn transport(message: String) -> Self {
        Self {
            message,
            status: None,
            data: None,
        }
    }

    /// `error.response?.data?.details?.[0]?.issue` (mis. `SUBSCRIPTION_STATUS_INVALID`).
    pub fn first_issue(&self) -> Option<&str> {
        self.data
            .as_ref()?
            .get("details")?
            .get(0)?
            .get("issue")?
            .as_str()
    }
}

impl std::fmt::Display for PaypalError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for PaypalError {}
