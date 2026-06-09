//! Implements [`drgtw_events::DeliveryLog`] for [`History`].
//!
//! This bridges the events crate (which owns the trait) and the history crate
//! (which owns the persistence layer) without a dependency cycle: events does
//! NOT import history; history imports events and extends it here.

use std::pin::Pin;

use drgtw_events::sink::{DeliveryLog, DeliveryRecord};

use crate::handle::History;
use crate::types::WebhookDeliveryRow;

impl DeliveryLog for History {
    fn record(
        &self,
        rec: DeliveryRecord,
    ) -> Pin<Box<dyn std::future::Future<Output = ()> + Send + '_>> {
        Box::pin(async move {
            let row = WebhookDeliveryRow {
                id: None,
                request_id: rec.request_id,
                ts_unix_ms: rec.ts_unix_ms,
                status_code: rec.status_code,
                ok: rec.ok,
                error: rec.error,
                attempt: rec.attempt,
                payload: rec.payload,
            };
            // Fire-and-forget: swallow errors (a logging failure must not
            // surface to the caller — the event has already been delivered or
            // dropped by the sink worker).
            let _ = self.record_webhook_delivery(&row).await;
        })
    }
}
