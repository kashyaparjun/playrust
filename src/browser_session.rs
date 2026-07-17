//! Persistent browser automation session boundary.

use std::collections::BTreeSet;
use std::path::Path;

use serde_json::Value;

use crate::browser::BrowserHost;
use crate::flow::CompiledFlow;
use crate::report::FlowReport;
use crate::runner::{RunOptions, SessionRuntime};

pub use crate::runner::{SessionInspection, SessionPage};

/// Owns persistent browser state while resetting execution-local state per submission.
pub struct BrowserSession {
    runtime: SessionRuntime,
}

impl BrowserSession {
    pub async fn open(host: &BrowserHost, flow: &CompiledFlow) -> anyhow::Result<Self> {
        Ok(Self {
            runtime: SessionRuntime::open(host, flow).await?,
        })
    }

    pub fn settings_match(&self, flow: &CompiledFlow) -> bool {
        self.runtime.settings_match(flow)
    }

    pub fn output(&self, name: &str) -> Option<&Value> {
        self.runtime.output(name)
    }

    pub fn output_names(&self) -> BTreeSet<String> {
        self.runtime.output_names()
    }

    pub async fn inspect(
        &self,
        host: &BrowserHost,
        accessibility: bool,
        screenshot_directory: Option<&Path>,
    ) -> anyhow::Result<SessionInspection> {
        self.runtime
            .inspect(host, accessibility, screenshot_directory)
            .await
    }

    pub async fn execute(
        &mut self,
        host: &BrowserHost,
        flow: &CompiledFlow,
        options: &RunOptions,
    ) -> anyhow::Result<FlowReport> {
        anyhow::ensure!(
            self.settings_match(flow),
            "viewport and geolocation must match the first submission"
        );
        Ok(self.runtime.execute(host, flow, options).await)
    }

    pub async fn close(self, host: &BrowserHost) -> anyhow::Result<()> {
        self.runtime.close(host).await
    }
}
