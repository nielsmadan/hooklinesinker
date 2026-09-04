use crate::consumers::Consumer;
use crate::protocol::StatusEvent;
use std::time::Duration;

pub trait HttpClient {
    fn post_json(&self, url: &str, body: &[u8]) -> Result<u16, String>;
}

impl<T: HttpClient + ?Sized> HttpClient for &T {
    fn post_json(&self, url: &str, body: &[u8]) -> Result<u16, String> {
        (**self).post_json(url, body)
    }
}

#[derive(Clone, Debug)]
pub struct SinkProblem {
    pub consumer: String,
    pub message: String,
}

pub struct SinkFanout<C: HttpClient> {
    client: C,
}

impl<C: HttpClient> SinkFanout<C> {
    pub fn new(client: C) -> Self {
        Self { client }
    }

    pub fn send(&self, event: &StatusEvent, consumers: &[Consumer]) -> Vec<SinkProblem> {
        let mut problems = Vec::new();
        let body = match serde_json::to_vec(event) {
            Ok(bytes) => bytes,
            Err(e) => {
                problems.push(SinkProblem {
                    consumer: "*".to_string(),
                    message: format!("failed to encode status event: {e}"),
                });
                return problems;
            }
        };
        for consumer in consumers {
            if !consumer.capabilities.iter().any(|c| c == "status") {
                continue;
            }
            let Some(sink) = &consumer.sink else {
                continue;
            };
            match self.client.post_json(sink, &body) {
                Ok(status) if (200..300).contains(&status) => {}
                Ok(status) => problems.push(SinkProblem {
                    consumer: consumer.name.clone(),
                    message: format!("sink responded with status {status}"),
                }),
                Err(message) => problems.push(SinkProblem {
                    consumer: consumer.name.clone(),
                    message,
                }),
            }
        }
        problems
    }
}

pub struct UreqHttpClient {
    agent: ureq::Agent,
}

impl UreqHttpClient {
    pub fn new() -> Self {
        let config = ureq::Agent::config_builder()
            .timeout_connect(Some(Duration::from_millis(200)))
            .timeout_recv_response(Some(Duration::from_millis(200)))
            .timeout_recv_body(Some(Duration::from_millis(200)))
            .build();
        Self {
            agent: config.into(),
        }
    }
}

impl Default for UreqHttpClient {
    fn default() -> Self {
        Self::new()
    }
}

impl HttpClient for UreqHttpClient {
    fn post_json(&self, url: &str, body: &[u8]) -> Result<u16, String> {
        match self
            .agent
            .post(url)
            .header("Content-Type", "application/json")
            .send(body)
        {
            Ok(response) => Ok(response.status().as_u16()),
            Err(ureq::Error::StatusCode(code)) => Ok(code),
            Err(e) => Err(e.to_string()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::{Agent, PROTOCOL_VERSION, Phase, SessionIdentity};
    use std::cell::RefCell;

    fn event() -> StatusEvent {
        StatusEvent {
            protocol: PROTOCOL_VERSION,
            binding_id: "binding-1".into(),
            agent: Agent::Claude,
            event: "PreToolUse".into(),
            phase: Phase::Working,
            running: true,
            observed_at: "2026-09-04T00:00:00Z".into(),
            session: SessionIdentity {
                id: "s".into(),
                cwd: "/tmp".into(),
                transcript_path: None,
            },
            process: None,
            terminal: None,
            tmux: None,
            git: None,
            remote_host: None,
        }
    }

    fn consumer(name: &str, capabilities: &[&str], sink: Option<&str>) -> Consumer {
        Consumer {
            name: name.to_string(),
            protocol: PROTOCOL_VERSION,
            capabilities: capabilities.iter().map(|c| c.to_string()).collect(),
            sink: sink.map(str::to_string),
        }
    }

    struct StubClient {
        response: Result<u16, String>,
        calls: RefCell<Vec<String>>,
    }

    impl StubClient {
        fn ok() -> Self {
            Self {
                response: Ok(200),
                calls: RefCell::new(Vec::new()),
            }
        }

        fn failing(message: &str) -> Self {
            Self {
                response: Err(message.to_string()),
                calls: RefCell::new(Vec::new()),
            }
        }

        fn non_2xx(status: u16) -> Self {
            Self {
                response: Ok(status),
                calls: RefCell::new(Vec::new()),
            }
        }
    }

    impl HttpClient for StubClient {
        fn post_json(&self, url: &str, _body: &[u8]) -> Result<u16, String> {
            self.calls.borrow_mut().push(url.to_string());
            self.response.clone()
        }
    }

    #[test]
    fn consumers_without_a_sink_are_skipped() {
        let client = StubClient::ok();
        let consumers = vec![consumer("ringleader", &["status"], None)];
        let problems = SinkFanout::new(&client).send(&event(), &consumers);
        assert!(problems.is_empty());
        assert!(client.calls.borrow().is_empty());
    }

    #[test]
    fn a_consumer_without_the_status_capability_is_skipped() {
        let client = StubClient::ok();
        let consumers = vec![consumer("future", &["raw"], Some("http://sink"))];
        let problems = SinkFanout::new(&client).send(&event(), &consumers);
        assert!(problems.is_empty());
        assert!(client.calls.borrow().is_empty());
    }

    #[test]
    fn a_non_2xx_response_becomes_a_sink_problem() {
        let client = StubClient::non_2xx(500);
        let consumers = vec![consumer("juggler", &["status"], Some("http://sink"))];
        let problems = SinkFanout::new(&client).send(&event(), &consumers);
        assert_eq!(problems.len(), 1);
        assert_eq!(problems[0].consumer, "juggler");
        assert!(problems[0].message.contains("500"));
    }

    #[test]
    fn a_transport_failure_becomes_a_sink_problem() {
        let client = StubClient::failing("connection refused");
        let consumers = vec![consumer("juggler", &["status"], Some("http://sink"))];
        let problems = SinkFanout::new(&client).send(&event(), &consumers);
        assert_eq!(problems.len(), 1);
        assert!(problems[0].message.contains("connection refused"));
    }
}
