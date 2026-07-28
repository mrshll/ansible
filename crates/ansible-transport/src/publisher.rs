//! The publish side: spool, then upload, then confirm.
//!
//! Backpressure policy, straight from the plan: uploads are a bounded queue; on
//! Worker failure, retry with backoff and keep buffering to the local spool. The
//! cursor stops advancing, viewers show "live tail stalled", and nothing is
//! dropped. Never drop-and-continue — order is the one invariant, and a visible
//! stall is strictly better than a silent gap.

use std::path::Path;
use std::time::Duration;

use ansible_capture::Chunk;
use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;

use crate::{Error, Result, Spool, now_ms};

pub struct PublisherConfig {
    pub base_url: String,
    pub session_id: String,
    pub publish_token: String,
    /// Attempts per chunk before giving up and leaving it spooled.
    pub max_attempts: u32,
    /// Base backoff; doubles per attempt.
    pub backoff: Duration,
}

impl PublisherConfig {
    #[must_use]
    pub fn new(base_url: impl Into<String>, session_id: impl Into<String>) -> Self {
        Self {
            base_url: base_url.into(),
            session_id: session_id.into(),
            publish_token: "spike-publish-token".into(),
            max_attempts: 5,
            backoff: Duration::from_millis(100),
        }
    }
}

pub struct Publisher {
    config: PublisherConfig,
    spool: Spool,
    agent: ureq::Agent,
    frames_sent: u64,
    chunks_confirmed: u64,
    retries: u64,
}

impl Publisher {
    /// # Errors
    /// Errors if the spool directory cannot be created.
    pub fn new(config: PublisherConfig, spool_root: &Path) -> Result<Self> {
        let spool = Spool::open(spool_root, &config.session_id)?;
        Ok(Self {
            spool,
            config,
            agent: ureq::Agent::new_with_defaults(),
            frames_sent: 0,
            chunks_confirmed: 0,
            retries: 0,
        })
    }

    fn url(&self, action: &str) -> String {
        format!("{}/v1/session/{}/{action}", self.config.base_url, self.config.session_id)
    }

    /// Send an ephemeral frame, best effort.
    ///
    /// Deliberately *not* retried and deliberately not spooled. A frame's only job
    /// is to arrive quickly; if it fails, the durable chunk carrying the same bytes
    /// is still coming, and the viewer will splice it by byte offset. Retrying here
    /// would spend latency budget re-sending data that is about to arrive anyway.
    ///
    /// # Errors
    /// Errors only if the request could not be constructed; a transport failure is
    /// reported as `Ok(false)`.
    pub fn publish_frame(&mut self, byte_start: u64, bytes: &[u8], at_ms: u64) -> Result<bool> {
        if bytes.is_empty() {
            return Ok(true);
        }
        let body = serde_json::json!({
            "t": "frame",
            "byte_start": byte_start,
            "byte_end": byte_start + bytes.len() as u64,
            "at_ms": at_ms,
            "b64": STANDARD.encode(bytes),
        });
        let sent = self
            .agent
            .post(self.url("frame"))
            .header("Authorization", &format!("Bearer {}", self.config.publish_token))
            .header("Content-Type", "application/json")
            .send_json(&body)
            .is_ok();
        if sent {
            self.frames_sent += 1;
        }
        Ok(sent)
    }

    /// Spool a chunk, then upload it with backoff until the Worker confirms.
    ///
    /// The spool write happens *first*. A chunk that exists only in flight is one a
    /// crash loses, and it cannot be reconstructed later because byte ranges are
    /// contiguous and nothing downstream can synthesize a missing range.
    ///
    /// # Errors
    /// Errors if the chunk cannot be spooled, or if every attempt failed — in which
    /// case the chunk remains spooled for a later [`Self::flush_spool`].
    pub fn publish_chunk(&mut self, chunk: &Chunk) -> Result<()> {
        self.spool.put(chunk)?;
        self.upload_spooled(chunk)
    }

    fn upload_spooled(&mut self, chunk: &Chunk) -> Result<()> {
        let body = chunk.to_jsonl()?;
        let mut delay = self.config.backoff;
        let mut last: Option<Error> = None;

        for attempt in 0..self.config.max_attempts {
            if attempt > 0 {
                self.retries += 1;
                std::thread::sleep(delay);
                delay *= 2;
            }
            match self.try_upload(&body) {
                Ok(()) => {
                    // Only now is the chunk durable, so only now may the spool
                    // forget it.
                    self.spool.remove(chunk.seq)?;
                    self.chunks_confirmed += 1;
                    return Ok(());
                }
                Err(Error::Worker { status, what, body })
                    if status == 400 || status == 401 || status == 409 =>
                {
                    // A 409 means the Worker's cursor disagrees with ours, and a 400
                    // means the chunk is malformed. Neither improves by being sent
                    // again, so fail loudly rather than burning the retry budget on
                    // a request that cannot succeed.
                    return Err(Error::Worker { status, what, body });
                }
                Err(e) => last = Some(e),
            }
        }
        Err(last.unwrap_or_else(|| {
            Error::Protocol(format!("chunk {} exhausted upload attempts", chunk.seq))
        }))
    }

    fn try_upload(&self, body: &str) -> Result<()> {
        let response = self
            .agent
            .post(self.url("chunk"))
            .header("Authorization", &format!("Bearer {}", self.config.publish_token))
            .header("Content-Type", "application/jsonl")
            .send(body);

        match response {
            Ok(_) => Ok(()),
            Err(ureq::Error::StatusCode(code)) => {
                Err(Error::Worker { what: "chunk".into(), status: code, body: String::new() })
            }
            Err(e) => Err(Error::Worker { what: "chunk".into(), status: 0, body: e.to_string() }),
        }
    }

    /// Re-upload everything still spooled, in stream order.
    ///
    /// This is the recovery path after a Worker outage and after a crash-restart.
    /// Ascending order is not cosmetic: the Worker rejects anything that is not the
    /// next expected chunk, so replaying out of order would stall the session that
    /// this call exists to unstall.
    ///
    /// # Errors
    /// Errors on the first chunk that cannot be re-uploaded, leaving it and
    /// everything after it spooled.
    pub fn flush_spool(&mut self) -> Result<u64> {
        let mut flushed = 0;
        for seq in self.spool.pending()? {
            let chunk = self.spool.get(seq)?;
            self.upload_spooled(&chunk)?;
            flushed += 1;
        }
        Ok(flushed)
    }

    /// Write the manifest that closes the transcript.
    ///
    /// # Errors
    /// Errors if the Worker rejects or cannot be reached.
    pub fn finalize(&self, redaction_version: u32) -> Result<()> {
        self.agent
            .post(format!("{}?redaction_version={redaction_version}", self.url("finalize")))
            .header("Authorization", &format!("Bearer {}", self.config.publish_token))
            .send_empty()
            .map_err(|e| Error::Worker {
                what: "finalize".into(),
                status: 0,
                body: e.to_string(),
            })?;
        Ok(())
    }

    #[must_use]
    pub fn frames_sent(&self) -> u64 {
        self.frames_sent
    }

    #[must_use]
    pub fn chunks_confirmed(&self) -> u64 {
        self.chunks_confirmed
    }

    /// Upload attempts beyond the first. Non-zero means the session stalled and
    /// recovered, which the measurements writeup should report rather than hide.
    #[must_use]
    pub fn retries(&self) -> u64 {
        self.retries
    }

    #[must_use]
    pub fn spool(&self) -> &Spool {
        &self.spool
    }

    /// Convenience for callers that need the clock the capture crate refuses to read.
    #[must_use]
    pub fn now_ms() -> u64 {
        now_ms()
    }
}
