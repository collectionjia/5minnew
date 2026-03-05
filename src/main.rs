mod config;
mod market { pub mod discoverer; }
mod monitor { pub mod orderbook; }
mod trading { pub mod executor; }
mod utils { pub mod logger; }

use anyhow::Result;
use chrono::Utc;
use dashmap::DashMap;
use futures::StreamExt;
use rust_decimal::Decimal;
use rust_decimal_macros::dec;
use std::time::Duration;
use tokio::time::sleep;
use tracing::{info, warn, debug};
use polymarket_client_sdk::types::{B256};

use config::Config;
use market::discoverer::{MarketDiscoverer, MarketInfo, FIVE_MIN_SECS};
use monitor::orderbook::OrderBookMonitor;
use trading::executor::TradingExecutor;

#[tokio::main]
async fn main() -> Result<()> {
    rustls::crypto::ring::default_provider().install_default().expect("Failed to install rustls crypto provider");
    utils::logger::init_logger()?;
    let cfg = Config::from_env()?;

    let executor = TradingExecutor::new(
        cfg.private_key.clone(),
        cfg.max_order_size_usdc,
        None,
        cfg.gtd_expiration_secs,
        cfg.arbitrage_order_type.clone(),
    ).await?;

    let discoverer = MarketDiscoverer::new(cfg.crypto_symbols.clone());

    loop {
        let ts = MarketDiscoverer::calculate_current_window_timestamp(Utc::now());
        let markets = discoverer.get_markets_for_timestamp(ts).await?;
        if markets.is_empty() {
            warn!("未找到市场，等待5秒后重试");
            sleep(Duration::from_secs(5)).await;
            continue;
        }

        let mut monitor = OrderBookMonitor::new();
        for m in &markets { let _ = monitor.subscribe_market(m); }
        let mut stream = monitor.create_orderbook_stream()?;

        let window_end = chrono::DateTime::from_timestamp(ts + FIVE_MIN_SECS, 0).unwrap_or_else(|| Utc::now());
        let one_dollar_attempted: DashMap<B256, bool> = DashMap::new();

        info!(count = markets.len(), "开始倒计时策略监控");

        while let Some(update) = stream.next().await { match update {
            Ok(book) => {
                if let Some(pair) = monitor.handle_book_update(book) {
                    let yes_best_ask = pair.yes_book.asks.last().map(|a| (a.price, a.size));
                    let no_best_ask = pair.no_book.asks.last().map(|a| (a.price, a.size));

                    let now = Utc::now();
                    let sec_to_end = (window_end - now).num_seconds();
                    let countdown_active = sec_to_end <= 10 && sec_to_end >= 5;
                    let sec_to_end_nonneg = sec_to_end.max(0);
                    let countdown_minutes = sec_to_end_nonneg / 60;
                    let countdown_seconds = sec_to_end_nonneg % 60;

                    let market_symbol = markets.iter().find(|m| m.market_id == pair.market_id).map(|m| m.crypto_symbol.as_str()).unwrap_or("");
                    let market_display = if !market_symbol.is_empty() { format!("{}预测市场", market_symbol) } else { markets.iter().find(|m| m.market_id == pair.market_id).map(|m| m.title.clone()).unwrap_or_else(|| "未知市场".to_string()) };

                    if countdown_active {
                        info!("倒计时:{}分{:02}秒 | 市场:{}", countdown_minutes, countdown_seconds, market_display);
                        if one_dollar_attempted.get(&pair.market_id).is_none() {
                            if let (Some((y_price, _)), Some((n_price, _))) = (yes_best_ask, no_best_ask) {
                                if y_price >= dec!(0.99) || n_price >= dec!(0.99) {
                                    debug!("价格>=0.99，倒计时策略跳过 | 市场:{}", market_display);
                                } else {
                                    let (chosen_token, chosen_price, side_str) = if y_price >= n_price { (pair.yes_book.asset_id, y_price, "YES") } else { (pair.no_book.asset_id, n_price, "NO") };
                                    let mut qty = (dec!(1.0) / chosen_price) * dec!(100.0);
                                    qty = qty.floor() / dec!(100.0);
                                    if qty < dec!(5.0) { qty = dec!(5.0); }
                                    one_dollar_attempted.insert(pair.market_id, true);
                                    info!("⏱️ 倒计时策略下单 | 市场:{} | 方向:{} | 价格:{:.4} | 份额:{:.2}", market_display, side_str, chosen_price, qty);
                                    let exec = &executor;
                                    let token = chosen_token; let price = chosen_price; let q = qty;
                                    tokio::spawn(async move {
                                        if let Err(e) = exec.buy_at_price(token, price, q).await { warn!("倒计时策略下单失败: {}", e); } else { info!("倒计时策略下单成功"); }
                                    });
                                }
                            }
                        }
                    }
                }
            }
            Err(e) => { warn!("订单簿更新错误: {}", e); break; }
        } }

        info!("当前窗口结束，刷新下一轮");
    }
}
