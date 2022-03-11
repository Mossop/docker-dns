use futures::StreamExt;
use std::{
    collections::HashMap,
    env,
    fs::File,
    path::{Path, PathBuf},
};
use tokio::sync::watch;
use trust_dns_server::client::rr::rdata::SOA;

use crate::{dns::Fqdn, dns::ServerConfig, dns::Upstream, sources::SourceConfig, watcher::watch};

mod file;

pub use file::deserialize_url;

#[derive(Debug)]
pub struct ZoneConfig {
    pub upstream: Option<Upstream>,
    pub ttl: u32,
    nameserver: Fqdn,
    pub authoratative: bool,
}

impl ZoneConfig {
    fn new(name: &Fqdn, defaults: &file::ZoneConfig) -> Self {
        Self {
            upstream: defaults.upstream.clone(),
            ttl: defaults.ttl.unwrap_or(300),
            nameserver: defaults
                .nameserver
                .clone()
                .unwrap_or_else(|| match name.domain() {
                    Some(n) => n.child("ns"),
                    None => name.clone().child("ns"),
                }),
            authoratative: defaults.authoratative.unwrap_or_default(),
        }
    }

    pub fn nameserver(&self) -> &Fqdn {
        &self.nameserver
    }

    pub fn hostmaster(&self) -> Fqdn {
        self.nameserver.child("hostmaster")
    }

    pub fn soa(&self) -> Option<SOA> {
        Some(SOA::new(
            self.nameserver().name()?.clone(),
            self.hostmaster().name()?.clone(),
            0,
            self.ttl.try_into().unwrap(),
            self.ttl.try_into().unwrap(),
            (self.ttl * 10).try_into().unwrap(),
            60,
        ))
    }

    fn apply_config(&mut self, config: &file::ZoneConfig) {
        if let Some(ref upstream) = config.upstream {
            self.upstream = Some(upstream.clone());
        }
        if let Some(ttl) = config.ttl {
            self.ttl = ttl;
        }
        if let Some(ref nameserver) = config.nameserver {
            self.nameserver = nameserver.clone();
        }
        if let Some(authoratative) = config.authoratative {
            self.authoratative = authoratative;
        }
    }
}

#[derive(Clone, Default, Debug, PartialEq, Eq)]
struct Zones {
    pub defaults: file::ZoneConfig,
    pub zones: Vec<(Fqdn, file::ZoneConfig)>,
}

impl Zones {
    fn new(defaults: file::ZoneConfig, mut zones: HashMap<Fqdn, file::ZoneConfig>) -> Self {
        let mut zones: Vec<(Fqdn, file::ZoneConfig)> = zones.drain().collect();
        zones.sort_by(|(n1, _), (n2, _)| n1.cmp(n2));

        Self { defaults, zones }
    }

    fn build_config(&self, name: &Fqdn) -> ZoneConfig {
        let mut config = ZoneConfig::new(name, &self.defaults);

        for (name, c) in &self.zones {
            if name.is_parent(name) {
                config.apply_config(c);
            }
        }

        config
    }
}

#[derive(Clone, Default, Debug, PartialEq, Eq)]
pub struct Config {
    pub config_file: PathBuf,
    pub server: ServerConfig,
    pub sources: SourceConfig,
    zones: Zones,
}

impl Config {
    pub fn from_file(config_file: &Path) -> Result<Config, String> {
        let f = File::open(config_file)
            .map_err(|e| format!("Failed to open file at {}: {}", config_file.display(), e))?;

        let config: file::ConfigFile = serde_yaml::from_reader(f)
            .map_err(|e| format!("Failed to parse configuration: {}", e))?;

        Ok(Config {
            config_file: config_file.to_owned(),
            server: config.server,
            sources: config.sources,
            zones: Zones::new(config.defaults, config.zones),
        })
    }

    pub fn path(&self, path: &Path) -> PathBuf {
        self.config_file.parent().unwrap().join(path)
    }

    pub fn default(config_file: &Path) -> Self {
        Config {
            config_file: config_file.to_owned(),
            ..Default::default()
        }
    }

    pub fn zone_config(&self, name: &Fqdn) -> ZoneConfig {
        self.zones.build_config(name)
    }
}

pub fn config_stream(args: &[String]) -> watch::Receiver<Config> {
    let config_file = config_file(args.get(1));
    log::info!("Reading configuration from {}.", config_file.display());
    let mut file_stream = watch(&config_file).unwrap();

    let mut config = Config::from_file(&config_file);

    let (sender, receiver) = watch::channel(match config {
        Ok(ref c) => c.clone(),
        Err(ref e) => {
            log::error!("{}", e);
            Config::default(&config_file)
        }
    });

    tokio::spawn(async move {
        loop {
            file_stream.next().await;

            let next_config = Config::from_file(&config_file);
            if next_config == config {
                continue;
            }

            config = next_config;

            let actual_config = match config {
                Ok(ref config) => config.clone(),
                Err(ref e) => {
                    log::error!("{}", e);
                    Config::default(&config_file)
                }
            };

            if let Err(e) = sender.send(actual_config) {
                log::error!("Failed to send updated config: {}", e);
                return;
            }
        }
    });

    receiver
}

fn config_file(arg: Option<&String>) -> PathBuf {
    if let Some(str) = arg {
        PathBuf::from(str).canonicalize().unwrap()
    } else if let Ok(value) = env::var("LOCALNS_CONFIG") {
        PathBuf::from(value).canonicalize().unwrap()
    } else {
        PathBuf::from("config.yaml").canonicalize().unwrap()
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn zone_config() {}
}