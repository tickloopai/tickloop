//! A Futu OpenD on a loopback port for whole-program tests: it answers every request of the
//! Options Tick with one AAPL.US expiration and a call and put at the 100 strike, over the real
//! frame format.

use std::{
    io::{Read, Write},
    net::{TcpListener, TcpStream},
};

use serde_json::{Value, json};

pub const EXPIRY: &str = "2099-01-16";

const INIT_CONNECT: u32 = 1001;
const GET_SNAPSHOT: u32 = 3203;
const GET_CHAIN: u32 = 3209;
const GET_EXPIRATIONS: u32 = 3224;
const UNDERLYING: &str = "AAPL";

/// Starts the server and returns its port; it serves until the test process exits.
pub fn start() -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    std::thread::spawn(move || {
        for connection in listener.incoming().flatten() {
            std::thread::spawn(move || serve(connection));
        }
    });
    port
}

/// A loopback port nothing listens on.
pub fn closed_port() -> u16 {
    TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

fn serve(mut connection: TcpStream) {
    loop {
        let mut header = [0; 44];
        if connection.read_exact(&mut header).is_err() {
            return;
        }
        let length = u32::from_le_bytes(header[12..16].try_into().unwrap()) as usize;
        let mut body = vec![0; length];
        if connection.read_exact(&mut body).is_err() {
            return;
        }
        let (protocol, serial, request) = tick_futu::decode_frame(&header, &body).unwrap();
        let response =
            serde_json::to_vec(&json!({"retType":0,"s2c":answer(protocol, &request["c2s"])}))
                .unwrap();
        let frame = tick_futu::encode_frame(protocol, serial, &response).unwrap();
        if connection.write_all(&frame).is_err() {
            return;
        }
    }
}

fn answer(protocol: u32, request: &Value) -> Value {
    match protocol {
        INIT_CONNECT => json!({"keepAliveInterval":10}),
        GET_EXPIRATIONS => {
            json!({"dateList":[{"strikeTime":EXPIRY,"optionExpiryDateDistance":30}]})
        }
        GET_CHAIN => json!({"optionChain":[{"strikeTime":EXPIRY,"option":[{
            "call":contract("AAPL990116C100000"),
            "put":contract("AAPL990116P100000"),
        }]}]}),
        GET_SNAPSHOT => json!({"snapshotList":request["securityList"]
            .as_array()
            .unwrap()
            .iter()
            .map(snapshot)
            .collect::<Vec<_>>()}),
        _ => panic!("unexpected OpenD protocol {protocol}"),
    }
}

fn contract(code: &str) -> Value {
    json!({"basic":{"security":{"market":11,"code":code}},
        "optionExData":{"strikeTime":EXPIRY,"strikePrice":100,"optionStandardType":1}})
}

fn snapshot(security: &Value) -> Value {
    let (last, bid, ask) = if security["code"] == UNDERLYING {
        (100.0, 99.9, 100.1)
    } else {
        (3.0, 2.9, 3.1)
    };
    json!({"basic":{"security":security,"curPrice":last,"bidPrice":bid,"askPrice":ask,
            "updateTime":"2026-09-24 15:55:00","volume":"1000"},
        "optionExData":{"impliedVolatility":30.0,"openInterest":"100"}})
}
