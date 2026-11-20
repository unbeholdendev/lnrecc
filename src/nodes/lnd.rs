use lightning_invoice::Currency;
use log::{info, warn};
use std::error::Error;
use tonic_lnd::lnrpc::payment::PaymentStatus;

use crate::{
    config::{ConfigJob, ValidConfig},
    lnurl::LnurlPaymentRequestResponse,
};

pub async fn get_currency(
    server_url: &str,
    cert_path: &str,
    macaroon_path: &str,
) -> Result<Currency, Box<dyn Error>> {
    let mut client = tonic_lnd::connect(server_url.to_owned(), cert_path, macaroon_path).await?;

    let info = client
        .lightning()
        .get_info(tonic_lnd::lnrpc::GetInfoRequest {})
        .await?
        .into_inner();

    let network = match info.chains.first() {
        Some(chain) => chain.network.as_str(),
        None => return Err("LND reported no chain".into()),
    };

    currency_from_network(network).ok_or_else(|| format!("Unknown network {}", network).into())
}

fn currency_from_network(network: &str) -> Option<Currency> {
    match network {
        "mainnet" => Some(Currency::Bitcoin),
        "testnet" | "testnet4" => Some(Currency::BitcoinTestnet),
        "signet" => Some(Currency::Signet),
        "regtest" => Some(Currency::Regtest),
        "simnet" => Some(Currency::Simnet),
        _ => None,
    }
}

pub async fn pay_invoice(
    invoice: LnurlPaymentRequestResponse,
    job: &ConfigJob,
    config: ValidConfig,
) -> Result<(), Box<dyn Error>> {
    let mut client = tonic_lnd::connect(
        config.server_url.to_owned(),
        config.cert_path.to_owned(),
        config.macaroon_path.to_owned(),
    )
    .await?;

    let payment_response = client
        .router()
        .send_payment_v2(tonic_lnd::routerrpc::SendPaymentRequest {
            payment_request: invoice.pr,
            timeout_seconds: 30,
            fee_limit_sat: job
                .max_fee_sats
                .unwrap_or((job.amount_sats as f32 * 0.05).ceil() as i64), // max 1% fee
            ..Default::default()
        })
        .await?;

    let mut payment_stream = payment_response.into_inner();

    while let Some(payment) = payment_stream.message().await? {
        let payment_status = PaymentStatus::from_i32(payment.status).unwrap();

        match payment_status {
            PaymentStatus::Succeeded => {
                info!("Payment succeeded!");

                if let Some(success_action) = &invoice.success_action {
                    if success_action.message.capacity() > 0 {
                        info!("Receiver replied with: {}", success_action.message);
                    }
                }
            }
            PaymentStatus::InFlight => {
                info!("Payment in progress...");
            }
            _ => {
                warn!("Payment failed due to: {:?}", payment.failure_reason());
            }
        }
    }

    Ok(())
}
