use metrics::counter;
use vector_core::internal_event::InternalEvent;

#[derive(Debug)]
pub struct EventStoreDbMetricsSendingError {
    pub count: usize,
    pub error: String,
}

impl InternalEvent for EventStoreDbMetricsSendingError {
    fn emit_logs(&self) {
        error!(
            message = "Sending metric error.",
            error = ?self.error,
            error_type = "stream_error",
            stage = "sending",
        );
    }

    fn emit_metrics(&self) {
        counter!(
            "component_errors_total", self.count as u64,
            "stage" => "sending",
            "error_type" => "stream_error",
        );
        counter!(
            "component_discarded_events_total", self.count as u64,
            "stage" => "sending",
            "error_type" => "stream_error",
        );
    }
}

#[derive(Debug)]
pub struct EventStoreDbMetricsHttpError {
    pub error: crate::Error,
}

impl InternalEvent for EventStoreDbMetricsHttpError {
    fn emit_logs(&self) {
        error!(
            message = "HTTP request processing error.",
            error = ?self.error,
            error_type = "http_error",
            stage = "receiving",
        );
    }

    fn emit_metrics(&self) {
        counter!(
            "component_errors_total", 1,
            "stage" => "receiving",
            "error_type" => "http_error",
        );
        // deprecated
        counter!("http_request_errors_total", 1);
    }
}

#[derive(Debug)]
pub struct EventStoreDbStatsParsingError {
    pub error: serde_json::Error,
}

impl InternalEvent for EventStoreDbStatsParsingError {
    fn emit_logs(&self) {
        error!(
            message = "JSON parsing error.",
            error = ?self.error,
            error_type = "parse_failed",
            stage = "processing",
        );
    }

    fn emit_metrics(&self) {
        counter!(
            "component_errors_total", 1,
            "stage" => "processing",
            "error_type" => "parse_failed",
        );
        // deprecated
        counter!("parse_errors_total", 1);
    }
}

pub struct EventStoreDbMetricsBytesReceived {
    pub byte_size: usize,
}

impl InternalEvent for EventStoreDbMetricsBytesReceived {
    fn emit_logs(&self) {
        trace!(
            message = "Bytes received.",
            byte_size = %self.byte_size,
            protocol = "http",
        );
    }

    fn emit_metrics(&self) {
        counter!(
            "component_received_bytes_total", self.byte_size as u64,
            "protocol" => "http",
        );
    }
}

pub struct EventStoreDbMetricsEventsReceived {
    pub count: usize,
    pub byte_size: usize,
}

impl InternalEvent for EventStoreDbMetricsEventsReceived {
    fn emit_logs(&self) {
        trace!(message = "Events received.", count = %self.count, byte_size = %self.byte_size);
    }

    fn emit_metrics(&self) {
        counter!("component_received_events_total", self.count as u64);
        counter!(
            "component_received_event_bytes_total",
            self.byte_size as u64
        );
        // deprecated
        counter!("events_in_total", self.count as u64);
        counter!("processed_bytes_total", self.byte_size as u64);
    }
}

pub struct EventStoreDbMetricsEventsSent {
    pub count: usize,
    pub byte_size: usize,
}

impl InternalEvent for EventStoreDbMetricsEventsSent {
    fn emit_logs(&self) {
        trace!(message = "Events sent.", count = %self.count, byte_size = %self.byte_size);
    }

    fn emit_metrics(&self) {
        counter!("component_sent_events_total", self.count as u64);
        counter!("component_sent_event_bytes_total", self.byte_size as u64);
    }
}
