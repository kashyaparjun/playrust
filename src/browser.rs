use std::path::Path;
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
use std::time::Duration;

use anyhow::{Context as _, Result, bail};
use chromiumoxide::Page;
use chromiumoxide::browser::{Browser, BrowserConfig};
use chromiumoxide::cdp::browser_protocol::browser::{BrowserContextId, GetVersionReturns};
use chromiumoxide::cdp::browser_protocol::emulation::SetDeviceMetricsOverrideParams;
use chromiumoxide::cdp::browser_protocol::target::{
    CreateBrowserContextParams, CreateTargetParams,
};
use chromiumoxide::handler::viewport::Viewport as ChromiumViewport;
use futures_util::StreamExt;
use tempfile::TempDir;
use tokio::sync::watch;
use tokio::task::JoinHandle;
use tokio::time::timeout;

const SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(5);
const MAX_VIEWPORT_DIMENSION: u32 = 10_000_000;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Viewport {
    pub width: u32,
    pub height: u32,
}

impl Viewport {
    pub fn new(width: u32, height: u32) -> Result<Self> {
        let viewport = Self { width, height };
        viewport.validate()?;
        Ok(viewport)
    }

    fn validate(self) -> Result<()> {
        if self.width == 0 || self.height == 0 {
            bail!("viewport dimensions must be greater than zero");
        }
        if self.width > MAX_VIEWPORT_DIMENSION || self.height > MAX_VIEWPORT_DIMENSION {
            bail!("viewport dimensions must not exceed {MAX_VIEWPORT_DIMENSION}");
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BrowserVersion {
    pub product: String,
    pub protocol_version: String,
    pub revision: String,
    pub user_agent: String,
    pub js_version: String,
}

impl From<GetVersionReturns> for BrowserVersion {
    fn from(version: GetVersionReturns) -> Self {
        Self {
            product: version.product,
            protocol_version: version.protocol_version,
            revision: version.revision,
            user_agent: version.user_agent,
            js_version: version.js_version,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum BrowserStatus {
    Running,
    Failed(String),
    Closed,
}

pub struct BrowserContext {
    id: BrowserContextId,
    page: Page,
    viewport: Viewport,
}

impl BrowserContext {
    pub fn id(&self) -> &BrowserContextId {
        &self.id
    }

    pub fn page(&self) -> &Page {
        &self.page
    }

    pub fn viewport(&self) -> Viewport {
        self.viewport
    }
}

pub struct BrowserHost {
    browser: Browser,
    version: BrowserVersion,
    handler_task: JoinHandle<()>,
    status_tx: watch::Sender<BrowserStatus>,
    status: watch::Receiver<BrowserStatus>,
    shutting_down: Arc<AtomicBool>,
    profile: TempDir,
}

impl BrowserHost {
    pub async fn launch(executable: impl AsRef<Path>, headed: bool) -> Result<Self> {
        let profile = tempfile::Builder::new()
            .prefix("playrust-chromium-")
            .tempdir()
            .context("create fresh Chromium profile")?;

        let mut config = BrowserConfig::builder()
            .chrome_executable(executable)
            .user_data_dir(profile.path())
            .viewport(Option::<ChromiumViewport>::None);
        if headed {
            config = config.with_head();
        }
        let config = config.build().map_err(anyhow::Error::msg)?;
        let (mut browser, mut handler) =
            Browser::launch(config).await.context("launch Chromium")?;

        let shutting_down = Arc::new(AtomicBool::new(false));
        let handler_shutting_down = Arc::clone(&shutting_down);
        let (status_tx, status) = watch::channel(BrowserStatus::Running);
        let handler_status_tx = status_tx.clone();
        let handler_task = tokio::spawn(async move {
            while let Some(result) = handler.next().await {
                if let Err(error) = result {
                    if !handler_shutting_down.load(Ordering::Acquire) {
                        handler_status_tx.send_replace(BrowserStatus::Failed(error.to_string()));
                    }
                    return;
                }
            }
            if !handler_shutting_down.load(Ordering::Acquire) {
                handler_status_tx.send_replace(BrowserStatus::Failed(
                    "Chromium connection handler stopped".to_owned(),
                ));
            }
        });

        let version = match browser.version().await {
            Ok(version) => version.into(),
            Err(error) => {
                shutting_down.store(true, Ordering::Release);
                let _ = timeout(SHUTDOWN_TIMEOUT, browser.close()).await;
                if !matches!(timeout(SHUTDOWN_TIMEOUT, browser.wait()).await, Ok(Ok(_))) {
                    let _ = timeout(SHUTDOWN_TIMEOUT, browser.kill()).await;
                }
                handler_task.abort();
                let _ = handler_task.await;
                return Err(error).context("read Chromium version");
            }
        };

        Ok(Self {
            browser,
            version,
            handler_task,
            status_tx,
            status,
            shutting_down,
            profile,
        })
    }

    pub fn browser(&self) -> &Browser {
        &self.browser
    }

    pub fn version(&self) -> &BrowserVersion {
        &self.version
    }

    pub fn status(&self) -> BrowserStatus {
        self.status.borrow().clone()
    }

    pub fn subscribe_status(&self) -> watch::Receiver<BrowserStatus> {
        self.status.clone()
    }

    pub async fn create_context(&self, viewport: Viewport) -> Result<BrowserContext> {
        viewport.validate()?;
        match self.status() {
            BrowserStatus::Running => {}
            BrowserStatus::Failed(error) => bail!("Chromium is unavailable: {error}"),
            BrowserStatus::Closed => bail!("Chromium is closed"),
        }

        let id = self
            .browser
            .create_browser_context(CreateBrowserContextParams::default())
            .await
            .context("create isolated Chromium browser context")?;
        let result = self.create_context_page(id.clone(), viewport).await;
        if result.is_err() {
            let _ = self.browser.dispose_browser_context(id).await;
        }
        result
    }

    async fn create_context_page(
        &self,
        id: BrowserContextId,
        viewport: Viewport,
    ) -> Result<BrowserContext> {
        let mut target = CreateTargetParams::new("about:blank");
        target.browser_context_id = Some(id.clone());
        let page = self
            .browser
            .new_page(target)
            .await
            .context("create page in isolated Chromium browser context")?;
        page.execute(SetDeviceMetricsOverrideParams::new(
            viewport.width,
            viewport.height,
            1.0,
            false,
        ))
        .await
        .context("apply fixed page viewport")?;

        Ok(BrowserContext { id, page, viewport })
    }

    pub async fn dispose_context(&self, context: BrowserContext) -> Result<()> {
        self.browser
            .dispose_browser_context(context.id)
            .await
            .context("dispose Chromium browser context")
    }

    pub async fn shutdown(mut self) -> Result<()> {
        self.shutting_down.store(true, Ordering::Release);
        self.status_tx.send_replace(BrowserStatus::Closed);

        let mut errors = Vec::new();
        match timeout(SHUTDOWN_TIMEOUT, self.browser.close()).await {
            Ok(Ok(_)) => {}
            Ok(Err(error)) => errors.push(format!("close Chromium: {error}")),
            Err(_) => errors.push("close Chromium timed out".to_owned()),
        }
        let exited = match timeout(SHUTDOWN_TIMEOUT, self.browser.wait()).await {
            Ok(Ok(_)) => true,
            Ok(Err(error)) => {
                errors.push(format!("wait for Chromium: {error}"));
                false
            }
            Err(_) => false,
        };
        if !exited {
            match timeout(SHUTDOWN_TIMEOUT, self.browser.kill()).await {
                Ok(Some(Ok(())) | None) => {}
                Ok(Some(Err(error))) => errors.push(format!("force Chromium to exit: {error}")),
                Err(_) => errors.push("force Chromium to exit timed out".to_owned()),
            }
        }

        self.handler_task.abort();
        if let Err(error) = self.handler_task.await
            && !error.is_cancelled()
        {
            errors.push(format!("join Chromium handler: {error}"));
        }

        if let Err(error) = self.profile.close() {
            errors.push(format!("remove temporary Chromium profile: {error}"));
        }
        if errors.is_empty() {
            Ok(())
        } else {
            bail!(errors.join("; "))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::Viewport;

    #[test]
    fn viewport_rejects_dimensions_chromium_cannot_emulate() {
        assert!(Viewport::new(0, 720).is_err());
        assert!(Viewport::new(1280, 10_000_001).is_err());
        assert_eq!(Viewport::new(1280, 720).unwrap().width, 1280);
    }
}
