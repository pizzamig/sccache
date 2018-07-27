// Copyright 2016 Mozilla Foundation
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

use directories::ProjectDirs;
use regex::Regex;
use std::env;
use std::fs::File;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::str::FromStr;
use toml;

lazy_static! {
    pub static ref CONFIG: Config = { Config::create() };
}

const ORGANIZATION: &str = "Mozilla";
const APP_NAME: &str = "sccache";
const TEN_GIGS: u64 = 10 * 1024 * 1024 * 1024;

pub fn default_disk_cache_dir() -> PathBuf {
    ProjectDirs::from("", ORGANIZATION, APP_NAME)
        .unwrap()
        .cache_dir()
        .to_owned()
}

fn parse_size(val: &str) -> Option<u64> {
    let re = Regex::new(r"^(\d+)([KMGT])$").unwrap();
    re.captures(val)
        .and_then(|caps| {
            caps.get(1)
                .and_then(|size| u64::from_str(size.as_str()).ok())
                .and_then(|size| Some((size, caps.get(2))))
        })
        .and_then(|(size, suffix)| match suffix.map(|s| s.as_str()) {
            Some("K") => Some(1024 * size),
            Some("M") => Some(1024 * 1024 * size),
            Some("G") => Some(1024 * 1024 * 1024 * size),
            Some("T") => Some(1024 * 1024 * 1024 * 1024 * size),
            _ => None,
        })
}

#[derive(Debug, PartialEq, Eq, Deserialize)]
pub struct AzureCacheConfig;

#[derive(Debug, PartialEq, Eq, Deserialize)]
pub struct DiskCacheConfig {
    pub dir: PathBuf,
    // TODO: use deserialize_with to allow human-readable sizes in toml
    pub size: u64,
}

impl Default for DiskCacheConfig {
    fn default() -> Self {
        DiskCacheConfig {
            dir: default_disk_cache_dir(),
            size: TEN_GIGS,
        }
    }
}

#[derive(Copy, Clone, Debug, PartialEq, Eq, Deserialize)]
pub enum GCSCacheRWMode {
    #[serde(rename = "READ_ONLY")]
    ReadOnly,
    #[serde(rename = "READ_WRITE")]
    ReadWrite,
}

#[derive(Debug, PartialEq, Eq, Deserialize)]
pub struct GCSCacheConfig {
    pub bucket: String,
    pub cred_path: Option<PathBuf>,
    pub rw_mode: GCSCacheRWMode,
}

#[derive(Debug, PartialEq, Eq, Deserialize)]
pub struct MemcachedCacheConfig {
    pub url: String,
}

#[derive(Debug, PartialEq, Eq, Deserialize)]
pub struct RedisCacheConfig {
    pub url: String,
}

#[derive(Debug, PartialEq, Eq, Deserialize)]
pub struct S3CacheConfig {
    pub bucket: String,
    pub endpoint: String,
}

#[derive(Debug, PartialEq, Eq)]
pub enum CacheType {
    Azure(AzureCacheConfig),
    GCS(GCSCacheConfig),
    Memcached(MemcachedCacheConfig),
    Redis(RedisCacheConfig),
    S3(S3CacheConfig),
}

#[derive(Debug, Default, Deserialize)]
pub struct CacheConfigs {
    azure: Option<AzureCacheConfig>,
    disk: Option<DiskCacheConfig>,
    gcs: Option<GCSCacheConfig>,
    memcached: Option<MemcachedCacheConfig>,
    redis: Option<RedisCacheConfig>,
    s3: Option<S3CacheConfig>,
}

impl CacheConfigs {
    /// Return a vec of the available cache types in an arbitrary but
    /// consistent ordering
    fn into_vec_and_fallback(self) -> (Vec<CacheType>, DiskCacheConfig) {
        let CacheConfigs {
            azure,
            disk,
            gcs,
            memcached,
            redis,
            s3,
        } = self;

        let caches = s3.map(CacheType::S3)
            .into_iter()
            .chain(redis.map(CacheType::Redis))
            .chain(memcached.map(CacheType::Memcached))
            .chain(gcs.map(CacheType::GCS))
            .chain(azure.map(CacheType::Azure))
            .collect();
        let fallback = disk.unwrap_or_else(Default::default);

        (caches, fallback)
    }

    /// Override self with any existing fields from other
    fn merge(&mut self, other: Self) {
        let CacheConfigs {
            azure,
            disk,
            gcs,
            memcached,
            redis,
            s3,
        } = other;

        if azure.is_some() {
            self.azure = azure
        }
        if disk.is_some() {
            self.disk = disk
        }
        if gcs.is_some() {
            self.gcs = gcs
        }
        if memcached.is_some() {
            self.memcached = memcached
        }
        if redis.is_some() {
            self.redis = redis
        }
        if s3.is_some() {
            self.s3 = s3
        }
    }
}

#[derive(Debug, Default, Deserialize)]
pub struct FileConfig {
    cache: CacheConfigs,
}

fn try_read_config_file(path: &Path) -> Option<FileConfig> {
    let mut file = File::open(path)
        .map_err(|e| debug!("Couldn't open config file: {}", e))
        .ok()?;

    let mut string = String::new();
    file.read_to_string(&mut string)
        .map_err(|e| warn!("Failed to read config file: {}", e))
        .ok()?;

    let toml: toml::Value = toml::from_str(&string)
        .map_err(|e| warn!("Failed to parse config as toml: {}", e))
        .ok()?;

    toml.try_into()
        .map_err(|e| {
            warn!("Invalid format of config: {}", e);
        })
        .ok()
}

#[derive(Debug)]
pub struct EnvConfig {
    cache: CacheConfigs,
}

fn config_from_env() -> EnvConfig {
    let s3 = env::var("SCCACHE_BUCKET").ok().map(|bucket| {
        let endpoint = match env::var("SCCACHE_ENDPOINT") {
            Ok(endpoint) => format!("{}/{}", endpoint, bucket),
            _ => match env::var("SCCACHE_REGION") {
                Ok(ref region) if region != "us-east-1" => {
                    format!("{}.s3-{}.amazonaws.com", bucket, region)
                }
                _ => format!("{}.s3.amazonaws.com", bucket),
            },
        };
        S3CacheConfig { bucket, endpoint }
    });

    let redis = env::var("SCCACHE_REDIS")
        .ok()
        .map(|url| RedisCacheConfig { url });

    let memcached = env::var("SCCACHE_MEMCACHED")
        .ok()
        .map(|url| MemcachedCacheConfig { url });

    let gcs = env::var("SCCACHE_GCS_BUCKET").ok().map(|bucket| {
        let cred_path = env::var_os("SCCACHE_GCS_KEY_PATH").map(|p| PathBuf::from(p));
        let rw_mode = match env::var("SCCACHE_GCS_RW_MODE").as_ref().map(String::as_str) {
            Ok("READ_ONLY") => GCSCacheRWMode::ReadOnly,
            Ok("READ_WRITE") => GCSCacheRWMode::ReadWrite,
            // TODO: unsure if these should warn during the configuration loading
            // or at the time when they're actually used to connect to GCS
            Ok(_) => {
                warn!("Invalid SCCACHE_GCS_RW_MODE-- defaulting to READ_ONLY.");
                GCSCacheRWMode::ReadOnly
            }
            _ => {
                warn!("No SCCACHE_GCS_RW_MODE specified-- defaulting to READ_ONLY.");
                GCSCacheRWMode::ReadOnly
            }
        };
        GCSCacheConfig {
            bucket,
            cred_path,
            rw_mode,
        }
    });

    let azure = env::var("SCCACHE_AZURE_CONNECTION_STRING")
        .ok()
        .map(|_| AzureCacheConfig);

    let disk = env::var_os("SCCACHE_DIR")
        .map(|p| PathBuf::from(p))
        .map(|dir| {
            let size: u64 = env::var("SCCACHE_CACHE_SIZE")
                .ok()
                .and_then(|v| parse_size(&v))
                .unwrap_or(TEN_GIGS);
            DiskCacheConfig { dir, size }
        });

    let cache = CacheConfigs {
        azure,
        disk,
        gcs,
        memcached,
        redis,
        s3,
    };

    EnvConfig { cache }
}

#[derive(Debug, Default, PartialEq, Eq)]
pub struct Config {
    pub caches: Vec<CacheType>,
    pub fallback_cache: DiskCacheConfig,
}

impl Config {
    pub fn create() -> Config {
        let env_conf = config_from_env();

        let file_conf_path = env::var_os("SCCACHE_CONF")
            .map(|p| PathBuf::from(p))
            .unwrap_or_else(|| {
                let dirs = ProjectDirs::from("", ORGANIZATION, APP_NAME).unwrap();
                dirs.config_dir().join("config")
            });
        let file_conf = try_read_config_file(&file_conf_path).unwrap_or_default();

        Config::from_env_and_file_configs(env_conf, file_conf)
    }

    fn from_env_and_file_configs(env_conf: EnvConfig, file_conf: FileConfig) -> Config {
        let mut conf_caches: CacheConfigs = Default::default();

        let FileConfig { cache } = file_conf;
        conf_caches.merge(cache);

        let EnvConfig { cache } = env_conf;
        conf_caches.merge(cache);

        let (caches, fallback_cache) = conf_caches.into_vec_and_fallback();
        Config {
            caches,
            fallback_cache,
        }
    }
}

#[test]
fn test_parse_size() {
    assert_eq!(None, parse_size(""));
    assert_eq!(None, parse_size("100"));
    assert_eq!(Some(2048), parse_size("2K"));
    assert_eq!(Some(10 * 1024 * 1024), parse_size("10M"));
    assert_eq!(Some(TEN_GIGS), parse_size("10G"));
    assert_eq!(Some(1024 * TEN_GIGS), parse_size("10T"));
}

#[test]
fn config_overrides() {
    let env_conf = EnvConfig {
        cache: CacheConfigs {
            azure: Some(AzureCacheConfig),
            disk: Some(DiskCacheConfig {
                dir: "/env-cache".into(),
                size: 5,
            }),
            redis: Some(RedisCacheConfig {
                url: "myotherredisurl".to_owned(),
            }),
            ..Default::default()
        },
    };

    let file_conf = FileConfig {
        cache: CacheConfigs {
            disk: Some(DiskCacheConfig {
                dir: "/file-cache".into(),
                size: 15,
            }),
            memcached: Some(MemcachedCacheConfig {
                url: "memurl".to_owned(),
            }),
            redis: Some(RedisCacheConfig {
                url: "myredisurl".to_owned(),
            }),
            ..Default::default()
        },
    };

    assert_eq!(
        Config::from_env_and_file_configs(env_conf, file_conf),
        Config {
            caches: vec![
                CacheType::Redis(RedisCacheConfig {
                    url: "myotherredisurl".to_owned(),
                }),
                CacheType::Memcached(MemcachedCacheConfig {
                    url: "memurl".to_owned(),
                }),
                CacheType::Azure(AzureCacheConfig),
            ],
            fallback_cache: DiskCacheConfig {
                dir: "/env-cache".into(),
                size: 5,
            },
        }
    );
}
