use tickloop_harness_protocol::ProviderConnection;

/// The product and region of `connection` with the host of `endpoint`, as the provider picker
/// lists a connection.
pub fn connection_label(connection: &ProviderConnection, endpoint: &str) -> String {
    let host = reqwest::Url::parse(endpoint)
        .ok()
        .and_then(|url| {
            let host = url.host_str()?;
            Some(
                url.port()
                    .map_or_else(|| host.to_owned(), |port| format!("{host}:{port}")),
            )
        })
        .unwrap_or_else(|| "invalid endpoint".into());
    connection.region.as_ref().map_or_else(
        || format!("{} ({host})", connection.product),
        |region| format!("{} · {} ({host})", connection.product, region.name),
    )
}
