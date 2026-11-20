use std::path::PathBuf;

pub use self::installation::BrowserFetcherInstallation;
pub use self::options::BrowserFetcherOptions;
use crate::error::{FetcherError, Result};
use crate::{BrowserHost, BrowserKind, BrowserVersion, BuildInfo, Platform, Runtime};

mod installation;
mod options;

/// A [`BrowserFetcher`] used to download and install a browser.
///
/// By default, downloads Chrome for Testing (Stable channel). Use
/// [`BrowserFetcherOptions`] to customize the browser kind, version, and platform.
///
/// # Example
///
/// Download the latest stable Chrome for Testing:
/// ```no_run
/// # async fn example() -> Result<(), Box<dyn std::error::Error>> {
/// use chromey_fetcher::{BrowserFetcher, BrowserFetcherOptions};
///
/// let fetcher = BrowserFetcher::new(BrowserFetcherOptions::default()?);
/// let installation = fetcher.fetch().await?;
/// println!("Executable: {}", installation.executable_path.display());
/// # Ok(())
/// # }
/// ```
pub struct BrowserFetcher {
    host: BrowserHost,
    path: PathBuf,
    platform: Platform,
    kind: BrowserKind,
    version: BrowserVersion,
}

impl BrowserFetcher {
    pub fn new(options: BrowserFetcherOptions) -> Self {
        Self {
            host: options.host,
            path: options.path,
            platform: options.platform,
            kind: options.kind,
            version: options.version,
        }
    }

    /// Fetches the browser, either locally if it was previously
    /// installed or remotely. If fetching remotely, the method can take a long
    /// time to resolve.
    ///
    /// This fails if the download or installation fails. The fetcher doesn't
    /// retry on network errors during download. If the installation fails,
    /// it might leave the cache in a bad state and it is advised to wipe it.
    pub async fn fetch(&self) -> Result<BrowserFetcherInstallation> {
        let build_info = self
            .version
            .resolve(self.kind, self.platform, &self.host)
            .await?;

        if !self.local(&build_info).await {
            self.download(&build_info).await?;
        }

        Ok(self.installation(build_info))
    }

    async fn local(&self, build_info: &BuildInfo) -> bool {
        let folder_path = self.folder_path(build_info);
        let executable_path = self
            .kind
            .executable(self.platform, build_info, &folder_path);
        Runtime::exists(&executable_path).await
    }

    async fn download(&self, build_info: &BuildInfo) -> Result<()> {
        let url = self
            .kind
            .download_url(self.platform, build_info, &self.host);
        let folder_path = self.folder_path(build_info);
        let archive_path = folder_path.with_extension("zip");

        Runtime::download_file(&url, &archive_path)
            .await
            .map_err(FetcherError::DownloadFailed)?;
        Runtime::unzip(archive_path, folder_path)
            .await
            .map_err(FetcherError::InstallFailed)?;

        Ok(())
    }

    fn folder_path(&self, build_info: &BuildInfo) -> PathBuf {
        let mut folder_path = self.path.clone();
        folder_path.push(self.platform.folder_name(build_info));
        folder_path
    }

    fn installation(&self, build_info: BuildInfo) -> BrowserFetcherInstallation {
        let folder_path = self.folder_path(&build_info);
        let executable_path = self
            .kind
            .executable(self.platform, &build_info, &folder_path);
        BrowserFetcherInstallation {
            folder_path,
            executable_path,
            build_info,
        }
    }
}
