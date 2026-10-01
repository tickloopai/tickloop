use longbridge::{TradeContext, trade::USAssetOverview};
use serde_json::{Value, json};

use crate::{
    error::{Error, Result},
    rate_limit::RateLimiter,
    sdk::sdk_error,
};

pub async fn snapshot(context: &TradeContext, limiter: &RateLimiter) -> Result<Value> {
    let overview = limiter
        .execute(|| context.us_asset_overview())
        .await
        .map_err(sdk_error)?;
    positions_response(&overview)
}

pub fn positions_response(overview: &USAssetOverview) -> Result<Value> {
    let stocks = overview
        .stock_list
        .iter()
        .map(|stock| {
            if stock.full_symbol.trim().is_empty() || stock.currency.trim().is_empty() {
                return Err(Error::network(
                    "US position is missing its symbol or currency",
                ));
            }
            Ok(json!({
                "symbol": stock.full_symbol,
                "symbol_name": stock.name,
                "quantity": stock.quantity,
                "available_quantity": null,
                "cost_price": stock.average_cost,
                "currency": stock.currency,
            }))
        })
        .collect::<Result<Vec<Value>>>()?;
    // US account_type identifies cash/margin, not live/paper trading.
    Ok(json!({"list": [{"account_channel": "unknown", "stock_info": stocks}]}))
}
