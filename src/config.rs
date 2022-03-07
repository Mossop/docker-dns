use futures::StreamExt;
use local_ip_address::local_ip;
use serde::{
    de::{self, Visitor},
    Deserialize, Deserializer,
};
use std::{
    collections::hash_map::Iter,
    collections::HashMap,
    env, fmt,
    fs::File,
    net::IpAddr,
    path::{Path, PathBuf},
    time::Duration,
};
use tokio::sync::mpsc;
use tokio_stream::wrappers::ReceiverStream;
use trust_dns_server::resolver::Name;

use crate::{
    debounce::Debounced,
    record::RecordSet,
    server::{ServerConfig, Zone},
    sources::{dhcp::DhcpConfig, docker::DockerConfig, traefik::TraefikConfig, SourceConfig},
    upstream::{Upstream, UpstreamConfig},
    watcher::watch,
};

const CONFIG_DEBOUNCE: Duration = Duration::from_millis(500);

struct NameVisitor;

impl<'de> Visitor<'de> for NameVisitor {
    type Value = Name;

    fn expecting(&self, formatter: &mut fmt::Formatter) -> fmt::Result {
        write!(formatter, "a string")
    }

    fn visit_str<E>(self, value: &str) -> Result<Self::Value, E>
    where
        E: de::Error,
    {
        Name::parse(value, None)
            .map(|mut n| {
                n.set_fqdn(true);
                n
            })
            .map_err(|e| E::custom(format!("{}", e)))
    }
}

pub fn deserialize_fqdn<'de, D>(de: D) -> Result<Name, D::Error>
where
    D: Deserializer<'de>,
{
    de.deserialize_str(NameVisitor)
}

#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
struct ZoneConfig {
    #[serde(default)]
    upstream: Option<UpstreamConfig>,

    #[serde(default)]
    authoratative: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Default, Deserialize)]
struct ConfigFile {
    ip_address: Option<IpAddr>,

    #[serde(default)]
    pub upstream: Option<UpstreamConfig>,

    #[serde(default)]
    pub server: ServerConfig,

    #[serde(default)]
    pub sources: SourceConfig,

    #[serde(default)]
    pub zones: HashMap<String, ZoneConfig>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Config {
    pub config_file: PathBuf,
    config: ConfigFile,
}

impl Config {
    pub fn from_file(config_file: &Path) -> Result<Config, String> {
        let f = File::open(config_file)
            .map_err(|e| format!("Failed to open file at {}: {}", config_file.display(), e))?;

        let config: ConfigFile = serde_yaml::from_reader(f)
            .map_err(|e| format!("Failed to parse configuration: {}", e))?;

        Ok(Config {
            config_file: config_file.to_owned(),
            config,
        })
    }

    pub fn path(&self, path: &Path) -> PathBuf {
        self.config_file.parent().unwrap().join(path)
    }

    pub fn default(config_file: &Path) -> Self {
        Config {
            config_file: config_file.to_owned(),
            config: ConfigFile::default(),
        }
    }

    pub fn server_config(&self) -> &ServerConfig {
        &self.config.server
    }

    pub fn docker_sources(&self) -> Iter<String, DockerConfig> {
        self.config.sources.docker.iter()
    }

    pub fn traefik_sources(&self) -> Iter<String, TraefikConfig> {
        self.config.sources.traefik.iter()
    }

    pub fn dhcp_sources(&self) -> Iter<String, DhcpConfig> {
        self.config.sources.dhcp.iter()
    }

    pub fn ip_address(&self) -> Result<IpAddr, String> {
        if let Some(ip) = self.config.ip_address {
            Ok(ip)
        } else {
            local_ip().map_err(|e| {
                format!(
                    "Unable to find local IP address, please specify in config: {}",
                    e
                )
            })
        }
    }

    pub fn upstream(&self) -> &Option<UpstreamConfig> {
        &self.config.upstream
    }

    fn zone(&self, domain: Name) -> Zone {
        let name = domain.to_string();
        let fqdn = String::from(name.trim_end_matches('.'));

        let config = self.config.zones.get(&fqdn);
        let upstream_config = match config {
            Some(c) => {
                if c.authoratative {
                    None
                } else {
                    c.upstream.as_ref().or(self.config.upstream.as_ref())
                }
            }
            None => self.config.upstream.as_ref(),
        };

        let upstream = upstream_config.map(|c| Upstream::new(&domain.to_string(), c));

        Zone::new(domain, upstream)
    }

    pub fn zones(&self, records: RecordSet) -> Vec<Zone> {
        let mut zones: HashMap<Name, Zone> = Default::default();

        for domain in self.config.zones.keys() {
            let name = Name::parse(&format!("{}.", domain), None).unwrap();
            let zone = self.zone(name.clone());
            zones.insert(name, zone);
        }

        for record in records {
            let domain = record.name.trim_to((record.name.num_labels() - 1).into());

            match zones.get_mut(&domain) {
                Some(zone) => zone.insert(record),
                None => {
                    let mut zone = self.zone(domain.clone());
                    zone.insert(record);
                    zones.insert(domain, zone);
                }
            };
        }

        zones.into_values().collect()
    }
}

pub fn config_stream(args: &[String]) -> Debounced<ReceiverStream<Config>> {
    let (sender, receiver) = mpsc::channel(5);
    let stream = Debounced::new(ReceiverStream::new(receiver), CONFIG_DEBOUNCE);
    let config_file = config_file(args.get(1));

    log::info!("Reading configuration from {}.", config_file.display(),);

    tokio::spawn(async move {
        let mut config = Config::from_file(&config_file);
        let mut file_stream = watch(&config_file).unwrap();

        loop {
            let actual_config = match config {
                Ok(ref config) => config.clone(),
                Err(ref e) => {
                    log::error!("{}", e);
                    Config::default(&config_file)
                }
            };

            if let Err(e) = sender.send(actual_config).await {
                log::error!("Failed to send updated config: {}", e);
                return;
            }

            loop {
                file_stream.next().await;

                let next_config = Config::from_file(&config_file);
                if next_config != config {
                    config = next_config;
                    break;
                }
            }
        }
    });

    stream
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
