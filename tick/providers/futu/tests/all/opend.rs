use std::sync::Arc;

use serde_json::json;
use tick_provider::options::OptionsApi;

#[test]
fn provider_json_parses_optional_fields_and_string_int64_without_inventing_quotes() {
    let quotes=tick_futu::parse_snapshots(&json!({"snapshotList":[{"basic":{"security":{"market":11,"code":"AAPL260925C100000"},"curPrice":3.0,"volume":"9007199254740993","updateTime":"2026-09-24 15:55:00"},"optionExData":{"impliedVolatility":88.1,"openInterest":"1200"}}]})).unwrap();
    assert_eq!(quotes[0].volume, Some(9_007_199_254_740_993));
    assert_eq!(quotes[0].open_interest, Some(1200));
    assert_eq!(quotes[0].implied_volatility_percent, Some(88.1));
    assert_eq!(quotes[0].bid, None);
    assert_eq!(quotes[0].midpoint(), None);
    assert!(
        tick_futu::parse_snapshots(
            &json!({"snapshotList":[{"basic":{"security":{"market":99,"code":"X"}}}]})
        )
        .is_err()
    );
}

#[test]
fn wire_header_matches_opend_little_endian_format_and_checks_integrity() {
    let body = br#"{"retType":0,"s2c":{}}"#;
    let frame = tick_futu::encode_frame(3224, 7, body).unwrap();
    assert_eq!(
        &frame[..12],
        &[b'F', b'T', 0x98, 0x0c, 0, 0, 1, 0, 7, 0, 0, 0]
    );
    assert_eq!(
        u32::from_le_bytes(frame[12..16].try_into().unwrap()) as usize,
        body.len()
    );
    assert_eq!(&frame[36..44], &[0; 8]);
    let header = frame[..44].try_into().unwrap();
    let (protocol, serial, decoded) = tick_futu::decode_frame(&header, &frame[44..]).unwrap();
    assert_eq!((protocol, serial), (3224, 7));
    assert_eq!(decoded["retType"], 0);
    let mut corrupt = body.to_vec();
    corrupt[1] = b'!';
    assert!(tick_futu::decode_frame(&header, &corrupt).is_err());
}

#[tokio::test]
async fn native_provider_reads_a_mock_opend_server_over_the_actual_tcp_protocol() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let server = tokio::spawn(async move {
        for connection in 0..4 {
            let (mut socket, _) = listener.accept().await.unwrap();
            for stage in 0..if connection == 0 { 1 } else { 2 } {
                let mut header = [0; 44];
                socket.read_exact(&mut header).await.unwrap();
                let length = u32::from_le_bytes(header[12..16].try_into().unwrap()) as usize;
                let mut body = vec![0; length];
                socket.read_exact(&mut body).await.unwrap();
                let (protocol, serial, request) = tick_futu::decode_frame(&header, &body).unwrap();
                let data = if stage == 0 {
                    assert_eq!(protocol, 1001);
                    assert_eq!(request["c2s"]["pushProtoFmt"], 1);
                    json!({"keepAliveInterval":10})
                } else {
                    match protocol {
                        3224 => {
                            assert_eq!(request["c2s"]["owner"], json!({"market":11,"code":"AAPL"}));
                            json!({"dateList":[{"strikeTime":"2026-09-25","optionExpiryDateDistance":1}]})
                        }
                        3209 => {
                            assert_eq!(request["c2s"]["beginTime"], "2026-09-25");
                            json!({"optionChain":[{"strikeTime":"2026-09-25","option":[{"call":{"basic":{"security":{"market":11,"code":"AAPL260925C100000"}},"optionExData":{"strikeTime":"2026-09-25","strikePrice":100,"optionStandardType":1}}}]}]})
                        }
                        3203 => {
                            assert_eq!(request["c2s"]["securityList"][0]["code"], "AAPL");
                            json!({"snapshotList":[{"basic":{"security":{"market":11,"code":"AAPL"},"curPrice":100,"bidPrice":99.9,"askPrice":100.1,"updateTime":"2026-09-24 15:55:00","volume":"1000"}}]})
                        }
                        _ => panic!("unexpected protocol {protocol}"),
                    }
                };
                let response = serde_json::to_vec(&json!({"retType":0,"s2c":data})).unwrap();
                socket
                    .write_all(&tick_futu::encode_frame(protocol, serial, &response).unwrap())
                    .await
                    .unwrap();
            }
        }
    });
    let api = Arc::new(
        tick_futu::FutuOptions::connect("127.0.0.1", port)
            .await
            .unwrap(),
    );
    assert_eq!(
        api.expirations("AAPL.US").await.unwrap()[0].date,
        "2026-09-25"
    );
    assert_eq!(
        api.chain("AAPL.US", "2026-09-25").await.unwrap()[0].strike,
        100.0
    );
    assert_eq!(
        api.snapshots(&["AAPL.US".into()]).await.unwrap()[0].last,
        Some(100.0)
    );
    server.await.unwrap();
}
