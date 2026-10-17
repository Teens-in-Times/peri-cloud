//! Internal structured Web entry. Browser authentication creates its identity;
//! it has no public message-ingress endpoint and performs no external delivery.
use crate::gateway::{Delivery, DeliveryError, MessageAdapter};
use async_trait::async_trait;
pub(crate) const WEB_ADAPTER: &str = "__peri_web";
pub(crate) struct WebAdapter;
#[async_trait]
impl MessageAdapter for WebAdapter {
    fn instance_id(&self) -> &str {
        WEB_ADAPTER
    }
    fn display_name(&self) -> &'static str {
        "网页任务入口"
    }
    fn connection_status(&self) -> (&'static str, Option<i64>) {
        ("browser_authenticated", None)
    }
    fn initial_access_policy(&self) -> crate::gateway::access::AccessPolicy {
        crate::gateway::access::AccessPolicy::open()
    }
    async fn deliver(&self, _delivery: &Delivery) -> Result<(), DeliveryError> {
        Ok(())
    }
}
