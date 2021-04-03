pub mod multiplexer;
pub mod station;
pub mod stationprovider;
use self::{
    station::{Station, Stations},
    stationprovider::StationProvider,
};
use crate::{
    config::Config, credentials::LocastCredentials, fcc_facilities::FCCFacilities, utils::get_async,
};
use chrono::Utc;
use futures::Future;
use log::info;
use regex::Regex;
use reqwest::Url;
use serde::Deserialize;
use serde_json::Value;
use std::{
    borrow::Cow,
    collections::HashMap,
    convert::{From, TryFrom},
    fmt,
    pin::Pin,
    str::FromStr,
    sync::{Arc, Mutex},
    thread, time,
};

static DMA_URL: &str = "https://api.locastnet.org/api/watch/dma";
static IP_URL: &str = "https://api.locastnet.org/api/watch/dma/ip";
static STATIONS_URL: &str = "https://api.locastnet.org/api/watch/epg";
static WATCH_URL: &str = "https://api.locastnet.org/api/watch/station";

#[derive(Debug)]
pub struct LocastService {
    config: Arc<Config>,
    credentials: Arc<LocastCredentials>,
    fcc_facilities: Arc<FCCFacilities>,
    pub zipcode: Option<String>,
    pub geo: Arc<Geo>,
    pub uuid: String,
    stations: Stations,
}

impl LocastService {
    pub fn new(
        config: Arc<Config>,
        credentials: Arc<LocastCredentials>,
        fcc_facilities: Arc<FCCFacilities>,
        zipcode: Option<String>,
    ) -> LocastServiceArc {
        let geo = Arc::new(Geo::from(&zipcode));
        if !geo.active {
            panic!(format!("{} not active", geo.name))
        }

        let uuid = uuid::Uuid::new_v5(
            &uuid::Uuid::from_str(&config.uuid).unwrap(),
            geo.DMA.as_bytes(),
        )
        .to_string();

        let stations = Arc::new(Mutex::new(build_stations(
            locast_stations(&geo.DMA, config.days, &credentials.token()),
            &geo,
            &config,
            &fcc_facilities,
        )));

        start_updater_thread(&config, &stations, &geo, &credentials, &fcc_facilities);

        let service = Arc::new(LocastService {
            config,
            credentials,
            fcc_facilities,
            zipcode,
            geo,
            uuid,
            stations,
        });

        service
    }

    // Convenience method for building stations based on &self
    fn build_stations(&self) -> Vec<Station> {
        let locast_stations =
            locast_stations(&self.geo.DMA, self.config.days, &self.credentials.token());
        build_stations(
            locast_stations,
            &self.geo,
            &self.config,
            &self.fcc_facilities,
        )
    }
}

pub type LocastServiceArc = Arc<LocastService>;

impl StationProvider for LocastServiceArc {
    fn stations(&self) -> Stations {
        if self.config.disable_station_cache {
            Arc::new(Mutex::new(self.build_stations()))
        } else {
            self.stations.clone()
        }
    }

    fn station_stream_uri(&self, id: String) -> Pin<Box<dyn Future<Output = String> + '_>> {
        let url = format!(
            "{}/{}/{}/{}",
            WATCH_URL, id, self.geo.latitude, self.geo.longitude
        );

        #[allow(non_snake_case)]
        #[derive(Deserialize, Debug)]
        struct WatchResponse {
            streamUrl: String,
        }

        let token = self.credentials.token().to_owned();

        let s = async move {
            let response: HashMap<String, Value> =
                get_async(&url, Some(&token)).await.json().await.unwrap();

            let stream_url = response.get("streamUrl").unwrap().as_str().unwrap();
            let m3u_data = get_async(stream_url, None).await.text().await.unwrap();
            let master_playlist = hls_m3u8::MasterPlaylist::try_from(m3u_data.as_str());

            // Make this nicer with a match
            if master_playlist.is_err() {
                stream_url.to_owned()
            } else {
                let mut vs = master_playlist.unwrap().variant_streams;
                vs.sort_by(|a, b| {
                    let ea = match a {
                        hls_m3u8::tags::VariantStream::ExtXStreamInf { stream_data, .. } => {
                            stream_data.bandwidth()
                        }
                        _ => 0,
                    };
                    let eb = match b {
                        hls_m3u8::tags::VariantStream::ExtXStreamInf { stream_data, .. } => {
                            stream_data.bandwidth()
                        }
                        _ => 0,
                    };
                    ea.cmp(&eb)
                });
                let variant = vs.pop().unwrap();

                let variant_url = match variant {
                    hls_m3u8::tags::VariantStream::ExtXStreamInf { uri, .. } => uri,
                    _ => Cow::Borrowed(""),
                };

                let full_url = Url::parse(&stream_url)
                    .unwrap()
                    .join(&variant_url.to_string())
                    .unwrap()
                    .to_string();

                full_url.to_string()
            }
        };
        Box::pin(s)
    }

    fn geo(&self) -> Arc<Geo> {
        self.geo.clone()
    }

    fn uuid(&self) -> String {
        self.uuid.to_owned()
    }

    fn zipcode(&self) -> String {
        if let Some(z) = &self.zipcode {
            z.to_owned()
        } else {
            "".to_string()
        }
    }

    fn services(&self) -> Vec<LocastServiceArc> {
        Vec::new()
    }
}

impl fmt::Display for LocastService {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(
            f,
            "LocastService{{ zipcode: {:?}, uuid: {:?}, geo: {:?} }}",
            self.zipcode, self.uuid, self.geo
        )
    }
}

fn start_updater_thread(
    config: &Arc<Config>,
    stations: &Stations,
    geo: &Arc<Geo>,
    credentials: &Arc<LocastCredentials>,
    fcc_facilities: &Arc<FCCFacilities>,
) {
    // Can this be done nicer?
    let thread_stations = stations.clone();
    let thread_config = config.clone();
    let thread_geo = geo.clone();
    let thread_credentials = credentials.clone();
    let thread_facilities = fcc_facilities.clone();
    let thread_timeout = config.cache_timeout.clone();

    thread::spawn(move || loop {
        thread::sleep(time::Duration::from_secs(thread_timeout));
        let ls = locast_stations(
            &thread_geo.DMA,
            thread_config.days,
            &thread_credentials.token(),
        );
        let new_stations = build_stations(ls, &thread_geo, &thread_config, &thread_facilities);
        let mut stations = thread_stations.lock().unwrap();
        *stations = new_stations;
    });
}

// Build stations
fn build_stations(
    locast_stations: Vec<Station>,
    geo: &Geo,
    config: &Arc<Config>,
    fcc_facilities: &Arc<FCCFacilities>,
) -> Vec<Station> {
    info!(
        "Loading stations for {} (cache: {}, cache timeout: {}, days: {})..",
        geo.name, !config.disable_station_cache, config.cache_timeout, config.days
    );

    let mut stations: Vec<Station> = Vec::new();

    for mut station in locast_stations.into_iter() {
        station.timezone = geo.timezone.to_owned();
        station.city = Some(geo.name.to_owned());

        let channel_from_call_sign = match Regex::new(r"(\d+\.\d+) .+")
            .unwrap()
            .captures(&station.callSign)
        {
            Some(c) => Some(c.get(1).map_or("", |m| m.as_str())),
            None => None,
        };

        let c = if let Some(channel) = channel_from_call_sign {
            Some(channel.to_string())
        } else if let Some((call_sign, sub_channel)) =
            detect_callsign(&station.name).or(detect_callsign(&station.callSign))
        {
            let dma = geo.DMA.parse::<i64>().unwrap();
            let channel = fcc_facilities.lookup(dma, call_sign, sub_channel);

            Some(channel)
        } else {
            panic!(format!(
                "Channel {}, call sign: {} not found!",
                &station.name, &station.callSign
            ));
        };
        station.channel = c;
        stations.push(station);
    }
    stations
}

fn locast_stations(dma: &str, days: u8, token: &str) -> Vec<Station> {
    let start_time = Utc::now().format("%Y-%m-%dT00:00:00-00:00").to_string();
    let uri = format!(
        "{}/{}?startTime={}&hours={}",
        STATIONS_URL,
        dma,
        start_time,
        days * 24
    );
    crate::utils::get(&uri, Some(token))
        .json::<Vec<Station>>()
        .unwrap()
}

fn detect_callsign(input: &str) -> Option<(&str, &str)> {
    let re = Regex::new(r"^([KW][A-Z]{2,3})[A-Z]{0,2}(\d{0,2})$").unwrap();
    let caps = re.captures(input)?;
    let call_sign = caps.get(1).map_or("default", |m| m.as_str());
    let sub_channel = caps.get(2).map_or("default", |m| m.as_str());
    Some((call_sign, sub_channel))
}

#[allow(non_snake_case)]
#[derive(Deserialize, Debug)]
pub struct Geo {
    pub latitude: f64,
    pub longitude: f64,
    pub DMA: String,
    pub name: String,
    pub active: bool,
    pub timezone: Option<String>,
}

impl From<&Option<String>> for Geo {
    fn from(zipcode: &Option<String>) -> Self {
        let uri = match zipcode {
            Some(z) => format!("{}/zip/{}", DMA_URL, z),
            None => String::from(IP_URL),
        };

        let mut geo = crate::utils::get(&uri, None).json::<Geo>().unwrap();
        geo.timezone = Some(tz_search::lookup(geo.latitude, geo.longitude).unwrap());
        geo
    }
}
