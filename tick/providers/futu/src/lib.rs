//! The Futu provider: option chains and quotes over a read-only OpenD JSON transport. Wire
//! fields follow FutunnOpen/py-futu-api and
//! https://openapi.futunn.com/futu-api-doc/ftapi/protocol.html.

use std::{collections::HashMap, net::IpAddr, time::Duration};

use async_trait::async_trait;
use serde_json::{Value, json};
use sha1::{Digest, Sha1};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpStream,
    sync::Mutex,
    time::Instant,
};

use tick_provider::{
    Interface,
    error::{Error, Result},
    options::{Contract, Expiration, MarketQuote, OptionSide, OptionsApi, normalize_symbol},
};

/// The `source` of the Tick Updates Futu data produces.
pub const SOURCE: &str = "futu";

pub const INTERFACES: &[Interface] = &[Interface::Options];

const INIT_CONNECT: u32 = 1001;
const GET_SNAPSHOT: u32 = 3203;
const GET_CHAIN: u32 = 3209;
const GET_EXPIRATIONS: u32 = 3224;
const HEADER_SIZE: usize = 44;
const MAX_BODY: usize = 8 * 1024 * 1024;
const REQUEST_TIMEOUT: Duration = Duration::from_secs(8);

pub struct FutuOptions {
    host: String,
    port: u16,
    last_requests: Mutex<HashMap<u32, Instant>>,
}

impl FutuOptions {
    pub async fn connect(host: &str, port: u16) -> Result<Self> {
        let client = Self::lazy(host, port)?;
        tokio::time::timeout(REQUEST_TIMEOUT, client.initialized_connection()).await.map_err(|_| Error::network("OpenD initialization timed out; check that FUTU HK OpenD is running and logged in"))??;
        Ok(client)
    }

    pub fn lazy(host: &str, port: u16) -> Result<Self> {
        if host != "localhost"
            && !host
                .parse::<IpAddr>()
                .is_ok_and(|address| address.is_loopback())
        {
            return Err(Error::usage(
                "native OpenD JSON currently supports unencrypted loopback connections only; use a local OpenD or a localhost tunnel",
            ));
        }
        if port == 0 {
            return Err(Error::usage("OpenD port must be nonzero"));
        }
        Ok(Self {
            host: host.into(),
            port,
            last_requests: Mutex::new(HashMap::new()),
        })
    }

    async fn initialized_connection(&self) -> Result<TcpStream> {
        let mut stream = TcpStream::connect((self.host.as_str(), self.port)).await.map_err(|error| Error::network(format!("cannot connect to Futu OpenD at {}:{}: {error}; start OpenD and log in with the FUTU HK account",self.host,self.port)))?;
        stream
            .set_nodelay(true)
            .map_err(|error| Error::network(error.to_string()))?;
        let client_id = format!(
            "tickloop-options-{}-{}",
            std::process::id(),
            time::OffsetDateTime::now_utc().unix_timestamp_nanos()
        );
        exchange(
            &mut stream,
            INIT_CONNECT,
            1,
            json!({"clientVer":100,"clientID":client_id,"recvNotify":false,"pushProtoFmt":1}),
        )
        .await?;
        Ok(stream)
    }

    async fn request(&self, protocol: u32, arguments: Value) -> Result<Value> {
        // Request-scoped connections close before the OpenD heartbeat deadline. A canceled
        // request drops its socket, so a partially consumed frame is never reused.
        let mut grants = self.last_requests.lock().await;
        let minimum_gap = match protocol {
            GET_CHAIN => Duration::from_millis(3100),
            GET_EXPIRATIONS => Duration::from_millis(1600),
            _ => Duration::from_millis(600),
        };
        if let Some(last) = grants.get(&protocol) {
            tokio::time::sleep_until(*last + minimum_gap).await;
        }
        grants.insert(protocol, Instant::now());
        drop(grants);
        tokio::time::timeout(REQUEST_TIMEOUT, async {
            let mut stream = self.initialized_connection().await?;
            exchange(&mut stream, protocol, 2, arguments).await
        })
        .await
        .map_err(|_| Error::network(format!("OpenD protocol {protocol} timed out")))?
    }
}

pub fn encode_frame(protocol: u32, serial: u32, body: &[u8]) -> Result<Vec<u8>> {
    if body.len() > MAX_BODY {
        return Err(Error::usage("OpenD request exceeds maximum packet size"));
    }
    let mut frame = vec![0; HEADER_SIZE];
    frame[..2].copy_from_slice(b"FT");
    frame[2..6].copy_from_slice(&protocol.to_le_bytes());
    frame[6] = 1;
    frame[8..12].copy_from_slice(&serial.to_le_bytes());
    frame[12..16].copy_from_slice(&(body.len() as u32).to_le_bytes());
    frame[16..36].copy_from_slice(&Sha1::digest(body));
    frame.extend_from_slice(body);
    Ok(frame)
}

pub fn decode_frame(header: &[u8; HEADER_SIZE], body: &[u8]) -> Result<(u32, u32, Value)> {
    let number = |range: std::ops::Range<usize>| {
        u32::from_le_bytes(header[range].try_into().expect("fixed-width header field"))
    };
    if &header[..2] != b"FT" || header[6] != 1 || header[7] != 0 {
        return Err(Error::network(
            "OpenD returned an unsupported header; JSON with no protocol encryption is required",
        ));
    }
    if number(12..16) as usize != body.len() || body.len() > MAX_BODY {
        return Err(Error::network("invalid OpenD body length"));
    }
    if Sha1::digest(body)[..] != header[16..36] {
        return Err(Error::network("OpenD response SHA1 mismatch"));
    }
    let value = serde_json::from_slice(body)
        .map_err(|error| Error::network(format!("invalid OpenD JSON: {error}")))?;
    Ok((number(2..6), number(8..12), value))
}

async fn exchange(
    stream: &mut TcpStream,
    protocol: u32,
    serial: u32,
    arguments: Value,
) -> Result<Value> {
    let body = serde_json::to_vec(&json!({"c2s":arguments}))
        .map_err(|error| Error::usage(error.to_string()))?;
    stream
        .write_all(&encode_frame(protocol, serial, &body)?)
        .await
        .map_err(|error| Error::network(format!("OpenD write failed: {error}")))?;
    for _ in 0..32 {
        let mut header = [0; HEADER_SIZE];
        stream.read_exact(&mut header).await.map_err(|error| {
            Error::network(format!(
                "OpenD read failed: {error}; this client requires unencrypted local OpenD JSON"
            ))
        })?;
        let length =
            u32::from_le_bytes(header[12..16].try_into().expect("fixed-width header field"))
                as usize;
        if length > MAX_BODY {
            return Err(Error::network("OpenD response body exceeds 8 MiB"));
        }
        let mut body = vec![0; length];
        stream
            .read_exact(&mut body)
            .await
            .map_err(|error| Error::network(error.to_string()))?;
        let (response_protocol, response_serial, response) = decode_frame(&header, &body)?;
        if response_protocol != protocol || response_serial != serial {
            continue;
        }
        if integer(&response["retType"]) != Some(0) {
            return Err(Error::network(format!(
                "Futu OpenD {protocol} rejected the request (code {}): {}. Verify the FUTU HK account's market/option quote permissions in OpenD.",
                response["errCode"],
                response["retMsg"]
                    .as_str()
                    .unwrap_or("missing success status")
            )));
        }
        return Ok(response.get("s2c").cloned().unwrap_or(Value::Null));
    }
    Err(Error::network(
        "too many unrelated OpenD packets before the requested reply",
    ))
}

fn security(symbol: &str) -> Result<Value> {
    let symbol = normalize_symbol(symbol)?;
    let (code, market) = symbol.rsplit_once('.').expect("validated symbol");
    Ok(json!({"market":if market=="US" {11} else {1},"code":code}))
}

fn symbol(value: &Value) -> Option<String> {
    let market = match integer(&value["market"])? {
        1 => "HK",
        11 => "US",
        _ => return None,
    };
    let code = value["code"].as_str()?;
    Some(format!("{code}.{market}"))
}

fn number(value: &Value) -> Option<f64> {
    value
        .as_f64()
        .or_else(|| value.as_str()?.parse().ok())
        .filter(|value| value.is_finite())
}

fn integer(value: &Value) -> Option<i64> {
    value.as_i64().or_else(|| value.as_str()?.parse().ok())
}
fn unsigned(value: &Value) -> Option<u64> {
    value.as_u64().or_else(|| value.as_str()?.parse().ok())
}

fn repeated<'a>(response: &'a Value, field: &str) -> Result<&'a [Value]> {
    if !response.is_object() {
        return Err(Error::network("OpenD response is missing the s2c object"));
    }
    match response.get(field) {
        None => Ok(&[]),
        Some(Value::Array(items)) => Ok(items),
        _ => Err(Error::network(format!("OpenD {field} is not a list"))),
    }
}

pub fn parse_expirations(response: &Value) -> Result<Vec<Expiration>> {
    let dates = repeated(response, "dateList")?;
    dates
        .iter()
        .map(|date| {
            let expiry = date["strikeTime"]
                .as_str()
                .ok_or_else(|| Error::network("OpenD expiration has no strikeTime"))?;
            if tick_provider::dates::parse_iso_date(expiry)
                .is_none_or(|date| tick_provider::dates::iso_date(date) != expiry)
            {
                return Err(Error::network("OpenD returned an invalid expiration date"));
            }
            Ok(Expiration {
                date: expiry.into(),
                days_to_expiry: integer(&date["optionExpiryDateDistance"]),
            })
        })
        .collect()
}

pub fn parse_chain(response: &Value) -> Result<Vec<Contract>> {
    let chains = repeated(response, "optionChain")?;
    let mut contracts = Vec::new();
    for chain in chains {
        for pair in chain["option"].as_array().into_iter().flatten() {
            for (field, side) in [("call", OptionSide::Call), ("put", OptionSide::Put)] {
                let Some(contract) = pair.get(field) else {
                    continue;
                };
                let extra = &contract["optionExData"];
                let symbol = symbol(&contract["basic"]["security"])
                    .ok_or_else(|| Error::network("OpenD contract has invalid security"))?;
                let strike = number(&extra["strikePrice"])
                    .filter(|price| *price > 0.0)
                    .ok_or_else(|| Error::network("OpenD contract has invalid strike"))?;
                let expiry = extra["strikeTime"]
                    .as_str()
                    .or_else(|| chain["strikeTime"].as_str())
                    .ok_or_else(|| Error::network("OpenD contract has no expiry"))?;
                contracts.push(Contract {
                    symbol,
                    strike,
                    expiry: expiry.into(),
                    side,
                    standard_type: integer(&extra["optionStandardType"]),
                });
            }
        }
    }
    Ok(contracts)
}

pub fn parse_snapshots(response: &Value) -> Result<Vec<MarketQuote>> {
    let snapshots = repeated(response, "snapshotList")?;
    snapshots
        .iter()
        .map(|snapshot| {
            let basic = &snapshot["basic"];
            let option = &snapshot["optionExData"];
            Ok(MarketQuote {
                symbol: symbol(&basic["security"])
                    .ok_or_else(|| Error::network("OpenD quote has invalid security"))?,
                last: number(&basic["curPrice"]),
                bid: number(&basic["bidPrice"]),
                ask: number(&basic["askPrice"]),
                implied_volatility_percent: number(&option["impliedVolatility"])
                    .filter(|value| *value >= 0.0),
                open_interest: unsigned(&option["openInterest"]),
                volume: unsigned(&basic["volume"]),
                quote_time: basic["updateTime"]
                    .as_str()
                    .filter(|text| !text.is_empty())
                    .map(str::to_owned),
                suspended: basic["isSuspend"].as_bool().unwrap_or(false),
            })
        })
        .collect()
}

#[async_trait]
impl OptionsApi for FutuOptions {
    fn source(&self) -> &'static str {
        SOURCE
    }
    async fn expirations(&self, symbol: &str) -> Result<Vec<Expiration>> {
        parse_expirations(
            &self
                .request(GET_EXPIRATIONS, json!({"owner":security(symbol)?}))
                .await?,
        )
    }
    async fn chain(&self, symbol: &str, expiry: &str) -> Result<Vec<Contract>> {
        parse_chain(
            &self
                .request(
                    GET_CHAIN,
                    json!({"owner":security(symbol)?,"beginTime":expiry,"endTime":expiry}),
                )
                .await?,
        )
    }
    async fn snapshots(&self, symbols: &[String]) -> Result<Vec<MarketQuote>> {
        if symbols.len() > 400 || symbols.is_empty() {
            return Err(Error::usage(
                "OpenD snapshot requests require 1..=400 symbols",
            ));
        }
        let securities = symbols
            .iter()
            .map(|symbol| security(symbol))
            .collect::<Result<Vec<_>>>()?;
        parse_snapshots(
            &self
                .request(GET_SNAPSHOT, json!({"securityList":securities}))
                .await?,
        )
    }
}
