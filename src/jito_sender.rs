use std::sync::atomic::{AtomicUsize, Ordering};

use base64::{engine::general_purpose, Engine as _};
use reqwest::Client;
use serde_json::json;

pub struct JitoSender {
    client: Client,
    bundle_url: String,
    uuids: Vec<String>,
    next_idx: AtomicUsize,
}

impl JitoSender {
    pub fn from_env() -> Option<Self> {
        let mut uuids = Vec::new();
        if let Ok(list) = std::env::var("JITO_UUIDS") {
            for u in list.split(',') {
                let s = u.trim();
                if !s.is_empty() {
                    uuids.push(s.to_string());
                }
            }
        }
        for key in ["JITO_UUID1", "JITO_UUID2", "JITO_UUID"] {
            if let Ok(val) = std::env::var(key) {
                let s = val.trim();
                if !s.is_empty() {
                    uuids.push(s.to_string());
                }
            }
        }
        if uuids.is_empty() {
            return None;
        }
        uuids.dedup();

        let bundle_url = std::env::var("JITO_BUNDLES_URL").unwrap_or_else(|_| {
            "https://tokyo.mainnet.block-engine.jito.wtf/api/v1/bundles".to_string()
        });

        Some(Self {
            client: Client::new(),
            bundle_url,
            uuids,
            next_idx: AtomicUsize::new(0),
        })
    }

    fn next_uuid(&self) -> String {
        let idx = self.next_idx.fetch_add(1, Ordering::Relaxed);
        let pos = idx % self.uuids.len();
        self.uuids[pos].clone()
    }

    pub async fn send_bundle_base64(&self, txs: &[Vec<u8>]) -> Result<(), String> {
        let uuid = self.next_uuid();
        let mut payloads = Vec::with_capacity(txs.len());
        for raw in txs {
            payloads.push(general_purpose::STANDARD.encode(raw));
        }
        let body = json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "sendBundle",
            "params": [payloads, { "encoding": "base64" }],
        });

        let resp = self
            .client
            .post(&self.bundle_url)
            .header("content-type", "application/json")
            .header("x-jito-auth", uuid)
            .json(&body)
            .send()
            .await
            .map_err(|err| format!("jito http error: {err}"))?;

        let status = resp.status();
        let text = resp
            .text()
            .await
            .map_err(|err| format!("jito read error: {err}"))?;
        if !status.is_success() {
            return Err(format!("jito status={status} body={text}"));
        }
        Ok(())
    }
}
