//! 插件素材目录、有限内存缓存与原子下载。业务素材的命名和更新时机由插件决定。
use std::{
    collections::HashMap,
    path::{Component, Path, PathBuf},
    sync::{Arc, LazyLock, Mutex, Weak},
    time::SystemTime,
};

use anyhow::{Result, ensure};
use tokio::{
    io::AsyncWriteExt,
    sync::{Mutex as AsyncMutex, Semaphore},
};

use crate::{
    PluginHttpClient,
    media::{download_limited, read_file_limited},
};

#[derive(Debug, Clone, Copy)]
pub struct AssetLimits {
    pub max_file_bytes: usize,
    pub memory_cache_bytes: usize,
    pub download_concurrency: usize,
}

impl Default for AssetLimits {
    fn default() -> Self {
        Self {
            max_file_bytes: 32 * 1024 * 1024,
            memory_cache_bytes: 16 * 1024 * 1024,
            download_concurrency: 2,
        }
    }
}

struct CacheEntry {
    bytes: Arc<[u8]>,
    modified: Option<SystemTime>,
    length: u64,
    used: u64,
}

#[derive(Default)]
struct MemoryCache {
    entries: HashMap<PathBuf, CacheEntry>,
    bytes: usize,
    clock: u64,
}

struct AssetsInner {
    root: PathBuf,
    http: PluginHttpClient,
    limits: AssetLimits,
    downloads: Arc<Semaphore>,
    locks: Mutex<HashMap<PathBuf, Weak<AsyncMutex<()>>>>,
    cache: Arc<Mutex<MemoryCache>>,
}

/// 从 `ctx.assets()` 获取；克隆共享缓存和同一文件的下载锁。
/// 默认位于 `data/<插件名>/assets/`，不会改写插件配置文件。
#[derive(Clone)]
pub struct PluginAssets(Arc<AssetsInner>);

impl std::fmt::Debug for PluginAssets {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PluginAssets")
            .field("root", &self.0.root)
            .field("limits", &self.0.limits)
            .finish_non_exhaustive()
    }
}

impl PluginAssets {
    pub fn new(root: impl Into<PathBuf>, http: PluginHttpClient) -> Self {
        static CACHE: LazyLock<Arc<Mutex<MemoryCache>>> =
            LazyLock::new(|| Arc::new(Mutex::new(MemoryCache::default())));
        static DOWNLOADS: LazyLock<Arc<Semaphore>> =
            LazyLock::new(|| Arc::new(Semaphore::new(AssetLimits::default().download_concurrency)));
        Self(Arc::new(AssetsInner {
            root: root.into(),
            http,
            limits: AssetLimits::default(),
            downloads: DOWNLOADS.clone(),
            cache: CACHE.clone(),
            locks: Mutex::new(HashMap::new()),
        }))
    }

    pub fn with_limits(
        root: impl Into<PathBuf>,
        http: PluginHttpClient,
        limits: AssetLimits,
    ) -> Result<Self> {
        ensure!(limits.max_file_bytes > 0, "素材大小上限必须大于 0");
        ensure!(
            limits.download_concurrency > 0
                && limits.download_concurrency <= Semaphore::MAX_PERMITS,
            "素材下载并发数超出范围"
        );
        Ok(Self(Arc::new(AssetsInner {
            root: root.into(),
            http,
            limits,
            downloads: Arc::new(Semaphore::new(limits.download_concurrency)),
            locks: Mutex::new(HashMap::new()),
            cache: Arc::new(Mutex::new(MemoryCache::default())),
        })))
    }

    pub fn root(&self) -> &Path {
        &self.0.root
    }

    pub fn path(&self, name: impl AsRef<Path>) -> Result<PathBuf> {
        let name = name.as_ref();
        ensure!(
            !name.as_os_str().is_empty()
                && name.components().all(|c| matches!(c, Component::Normal(_))),
            "素材名必须是素材目录内的相对路径"
        );
        Ok(self.0.root.join(name))
    }

    fn lock(&self, path: &Path) -> Arc<AsyncMutex<()>> {
        let mut locks = self.0.locks.lock().expect("锁已中毒");
        // 弱引用不会保留闲置锁，定期删除键，避免素材数量增长导致锁表常驻。
        locks.retain(|_, value| value.strong_count() > 0);
        if let Some(lock) = locks.get(path).and_then(Weak::upgrade) {
            return lock;
        }
        let lock = Arc::new(AsyncMutex::new(()));
        locks.insert(path.to_path_buf(), Arc::downgrade(&lock));
        lock
    }

    /// 读取素材字节。磁盘修改后不使用旧缓存；超出缓存预算的素材仍可读取但不常驻缓存。
    pub async fn read(&self, name: impl AsRef<Path>) -> Result<Arc<[u8]>> {
        let path = self.path(name)?;
        let lock = self.lock(&path);
        let _guard = lock.lock().await;
        let metadata = tokio::fs::metadata(&path).await?;
        let modified = metadata.modified().ok();
        {
            let mut cache = self.0.cache.lock().expect("锁已中毒");
            cache.clock = cache.clock.wrapping_add(1);
            let used = cache.clock;
            if let Some(entry) = cache.entries.get_mut(&path)
                && modified.is_some()
                && entry.modified == modified
                && entry.length == metadata.len()
            {
                entry.used = used;
                return Ok(entry.bytes.clone());
            }
        }
        let bytes: Arc<[u8]> = read_file_limited(&path, self.0.limits.max_file_bytes)
            .await?
            .into();
        let current = tokio::fs::metadata(&path).await?;
        if modified.is_some()
            && current.modified().ok() == modified
            && current.len() == metadata.len()
        {
            let mut cache = self.0.cache.lock().expect("锁已中毒");
            if let Some(old) = cache.entries.remove(&path) {
                cache.bytes -= old.bytes.len();
            }
            if bytes.len() <= self.0.limits.memory_cache_bytes {
                while cache.bytes.saturating_add(bytes.len()) > self.0.limits.memory_cache_bytes {
                    let oldest = cache
                        .entries
                        .iter()
                        .min_by_key(|(_, entry)| entry.used)
                        .map(|(path, _)| path.clone());
                    let Some(oldest) = oldest else {
                        break;
                    };
                    if let Some(old) = cache.entries.remove(&oldest) {
                        cache.bytes -= old.bytes.len();
                    }
                }
                cache.clock = cache.clock.wrapping_add(1);
                let used = cache.clock;
                cache.bytes += bytes.len();
                cache.entries.insert(
                    path,
                    CacheEntry {
                        bytes: bytes.clone(),
                        modified,
                        length: metadata.len(),
                        used,
                    },
                );
            }
        }
        Ok(bytes)
    }

    /// 仅在缺失时安装插件自带的默认素材，保留用户已有文件。
    pub async fn install(&self, name: impl AsRef<Path>, bytes: &[u8]) -> Result<PathBuf> {
        self.validate_bytes(bytes)?;
        let path = self.path(name)?;
        let lock = self.lock(&path);
        let _guard = lock.lock().await;
        if !self.exists(&path).await? {
            self.write(&path, bytes).await?;
        }
        Ok(path)
    }

    /// 用准备好的字节显式替换已有素材文件。
    /// 若校验或写入失败，原文件保持完好；写入成功后通过原子重命名生效并立即使内存缓存失效。
    /// 若安装插件内置的默认素材且不希望覆盖用户已有的修改，请改用 [`Self::install`]。
    pub async fn replace(&self, name: impl AsRef<Path>, bytes: &[u8]) -> Result<PathBuf> {
        self.validate_bytes(bytes)?;
        let path = self.path(name)?;
        let lock = self.lock(&path);
        let _guard = lock.lock().await;
        self.write(&path, bytes).await?;
        Ok(path)
    }

    /// 优先使用磁盘素材，缺失时下载；同一素材的并发请求只下载一次。
    pub async fn fetch(&self, name: impl AsRef<Path>, url: &str) -> Result<PathBuf> {
        self.download(name.as_ref(), url, false).await
    }

    /// 显式更新素材。下载失败时保留原文件，不用 HTTP 错误页覆盖素材。
    pub async fn refresh(&self, name: impl AsRef<Path>, url: &str) -> Result<PathBuf> {
        self.download(name.as_ref(), url, true).await
    }

    async fn download(&self, name: &Path, url: &str, refresh: bool) -> Result<PathBuf> {
        let path = self.path(name)?;
        let lock = self.lock(&path);
        let _guard = lock.lock().await;
        if !refresh && self.exists(&path).await? {
            return Ok(path);
        }
        let _permit = self.0.downloads.acquire().await?;
        let bytes = download_limited(self.0.http.raw(), url, self.0.limits.max_file_bytes).await?;
        self.write(&path, &bytes).await?;
        Ok(path)
    }

    async fn exists(&self, path: &Path) -> Result<bool> {
        match tokio::fs::metadata(path).await {
            Ok(meta) => {
                ensure!(
                    meta.is_file()
                        && meta.len() > 0
                        && meta.len() <= self.0.limits.max_file_bytes as u64,
                    "已有素材为空、过大或不是普通文件"
                );
                Ok(true)
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
            Err(error) => Err(error.into()),
        }
    }

    fn validate_bytes(&self, bytes: &[u8]) -> Result<()> {
        ensure!(
            !bytes.is_empty() && bytes.len() <= self.0.limits.max_file_bytes,
            "素材为空或超过大小上限"
        );
        Ok(())
    }

    async fn write(&self, path: &Path, bytes: &[u8]) -> Result<()> {
        self.validate_bytes(bytes)?;
        let parent = path.parent().expect("素材有父目录");
        tokio::fs::create_dir_all(parent).await?;
        let temporary =
            TemporaryFile(parent.join(format!(".nuono-asset-{}.tmp", uuid::Uuid::new_v4())));
        let mut file = tokio::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temporary.0)
            .await?;
        file.write_all(bytes).await?;
        file.flush().await?;
        drop(file);
        tokio::fs::rename(&temporary.0, path).await?;
        let mut cache = self.0.cache.lock().expect("锁已中毒");
        if let Some(old) = cache.entries.remove(path) {
            cache.bytes -= old.bytes.len();
        }
        Ok(())
    }
}

struct TemporaryFile(PathBuf);
impl Drop for TemporaryFile {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}
