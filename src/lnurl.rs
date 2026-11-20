use std::{
    error::Error,
    str::{from_utf8, FromStr},
};

use bitcoin::hashes::{sha256, Hash};
use lightning_invoice::{Bolt11Invoice, Bolt11InvoiceDescriptionRef, Currency};
use serde::Deserialize;

use crate::config::ConfigJob;

#[derive(Deserialize, Clone)]
#[serde(rename_all = "camelCase")]
struct LnurlInfoResponse {
    callback: String,
    metadata: String,
    //  max_sendable: u64,
    //  min_sendable: u64,
    //  comment_allowed: u64,
    // ...
}

#[derive(Deserialize, Clone)]
pub struct LnurlResponseSuccessAction {
    //  tag: String,
    pub message: String,
}

#[derive(Deserialize, Clone)]
#[serde(rename_all = "camelCase")]
pub struct LnurlPaymentRequestResponse {
    pub pr: String,
    //  routes: Vec<String>,
    pub success_action: Option<LnurlResponseSuccessAction>,
}

pub struct LnurlService {
    config_job: ConfigJob,
    currency: Currency,
    info: Option<LnurlInfoResponse>,
    payment_request: Option<LnurlPaymentRequestResponse>,
}

impl LnurlService {
    pub fn new(config_job: ConfigJob, currency: Currency) -> Self {
        LnurlService {
            config_job,
            currency,
            info: None,
            payment_request: None,
        }
    }

    async fn get_info(&mut self, url: &str) -> Result<&mut Self, Box<dyn Error>> {
        let lnurl_init_response = client()?
            .get(check_url(url)?)
            .send()
            .await?
            .json::<LnurlInfoResponse>()
            .await?;

        self.info = Some(lnurl_init_response);

        Ok(self)
    }

    async fn get_payment_request(&mut self) -> Result<&mut Self, Box<dyn Error>> {
        let info = match &self.info {
            Some(info) => info,
            None => return Err("No callback found".into()),
        };

        let amount_msats = u64::from(self.config_job.amount_sats) * 1000;

        let mut request_url = check_url(&info.callback)?;
        request_url
            .query_pairs_mut()
            .append_pair("amount", &amount_msats.to_string());
        if let Some(memo) = &self.config_job.memo {
            request_url.query_pairs_mut().append_pair("comment", memo);
        }

        let response = client()?
            .get(request_url)
            .send()
            .await?
            .json::<LnurlPaymentRequestResponse>()
            .await?;

        check_invoice(&response.pr, amount_msats, &info.metadata, &self.currency)?;

        self.payment_request = Some(response);

        Ok(self)
    }

    pub async fn get_invoice(
        &mut self,
        url: &str,
    ) -> Result<LnurlPaymentRequestResponse, Box<dyn Error>> {
        self.get_info(url).await?;
        self.get_payment_request().await?;

        match self.payment_request.clone() {
            Some(payment_request) => Ok(payment_request),
            None => Err("No payment request found".into()),
        }
    }
}

pub fn get_url_from_ln_address_or_lnurl(ln_address_or_lnurl: &str) -> String {
    if ln_address_or_lnurl.contains("@") {
        let url_parts: Vec<&str> = ln_address_or_lnurl.split("@").collect();

        format!(
            "https://{}/.well-known/lnurlp/{}",
            url_parts[1], url_parts[0]
        )
    } else {
        let lnurl_upper = ln_address_or_lnurl.to_uppercase();

        let (_hrp, data) = bech32::decode(&lnurl_upper).expect("Failed to decode lnurl");

        let url = from_utf8(&data).expect("Failed to convert lnurl bytes to utf8");

        url.to_string()
    }
}

fn client() -> reqwest::Result<reqwest::Client> {
    let policy = reqwest::redirect::Policy::custom(|attempt| {
        if attempt.previous().len() >= 10 {
            attempt.error("too many redirects")
        } else if let Err(err) = check_url(attempt.url().as_str()) {
            attempt.error(err)
        } else {
            attempt.follow()
        }
    });

    reqwest::Client::builder().redirect(policy).build()
}

fn check_url(url: &str) -> Result<reqwest::Url, String> {
    let parsed = reqwest::Url::parse(url).map_err(|e| format!("Invalid URL {}: {}", url, e))?;
    let onion = parsed
        .host_str()
        .is_some_and(|host| host.ends_with(".onion"));

    match parsed.scheme() {
        "https" => Ok(parsed),
        "http" if onion => Ok(parsed),
        _ => Err(format!("Refusing non-https URL {}", url)),
    }
}

fn check_invoice(
    pr: &str,
    amount_msats: u64,
    metadata: &str,
    currency: &Currency,
) -> Result<(), String> {
    let invoice =
        Bolt11Invoice::from_str(pr).map_err(|e| format!("Invalid invoice {}: {}", pr, e))?;

    if invoice.currency() != *currency {
        return Err(format!(
            "Invoice is for {:?}, node is on {:?}",
            invoice.currency(),
            currency
        ));
    }

    if invoice.amount_milli_satoshis() != Some(amount_msats) {
        return Err(format!(
            "Invoice amount {:?} msats differs from requested {} msats",
            invoice.amount_milli_satoshis(),
            amount_msats
        ));
    }

    let metadata_hash = sha256::Hash::hash(metadata.as_bytes());

    match invoice.description() {
        Bolt11InvoiceDescriptionRef::Hash(hash) if hash.0 == metadata_hash => Ok(()),
        _ => Err("Invoice description hash does not match LNURL metadata".to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bitcoin::hashes::{sha256, Hash};
    use bitcoin::secp256k1::{Message, Secp256k1, SecretKey};
    use lightning_invoice::{
        Bolt11InvoiceDescription, Description, InvoiceBuilder, PaymentSecret, Sha256,
    };

    const METADATA: &str = r#"[["text/plain","pay nick"]]"#;
    const MSATS: u64 = 10_000_000;

    fn invoice(
        currency: Currency,
        msats: Option<u64>,
        description: Bolt11InvoiceDescription,
    ) -> String {
        let secp = Secp256k1::new();
        let key = SecretKey::from_slice(&[7; 32]).unwrap();
        let sign = |hash: &Message| secp.sign_ecdsa_recoverable(hash, &key);
        let builder = InvoiceBuilder::new(currency)
            .invoice_description(description)
            .payment_hash(sha256::Hash::hash(b"preimage"))
            .payment_secret(PaymentSecret([1; 32]))
            .current_timestamp()
            .min_final_cltv_expiry_delta(144);
        match msats {
            Some(msats) => builder.amount_milli_satoshis(msats).build_signed(sign),
            None => builder.build_signed(sign),
        }
        .unwrap()
        .to_string()
    }

    fn hashed(metadata: &str) -> Bolt11InvoiceDescription {
        Bolt11InvoiceDescription::Hash(Sha256(sha256::Hash::hash(metadata.as_bytes())))
    }

    #[test]
    fn accepts_matching_invoice() {
        let pr = invoice(Currency::Bitcoin, Some(MSATS), hashed(METADATA));
        assert_eq!(
            check_invoice(&pr, MSATS, METADATA, &Currency::Bitcoin),
            Ok(())
        );
    }

    #[test]
    fn rejects_other_amount() {
        let pr = invoice(Currency::Bitcoin, Some(MSATS * 100), hashed(METADATA));
        assert!(check_invoice(&pr, MSATS, METADATA, &Currency::Bitcoin).is_err());
    }

    #[test]
    fn rejects_invoice_without_amount() {
        let pr = invoice(Currency::Bitcoin, None, hashed(METADATA));
        assert!(check_invoice(&pr, MSATS, METADATA, &Currency::Bitcoin).is_err());
    }

    #[test]
    fn rejects_other_description_hash() {
        let pr = invoice(Currency::Bitcoin, Some(MSATS), hashed("[]"));
        assert!(check_invoice(&pr, MSATS, METADATA, &Currency::Bitcoin).is_err());
    }

    #[test]
    fn rejects_plain_description() {
        let description =
            Bolt11InvoiceDescription::Direct(Description::new(METADATA.to_string()).unwrap());
        let pr = invoice(Currency::Bitcoin, Some(MSATS), description);
        assert!(check_invoice(&pr, MSATS, METADATA, &Currency::Bitcoin).is_err());
    }

    #[test]
    fn rejects_other_network() {
        let pr = invoice(Currency::BitcoinTestnet, Some(MSATS), hashed(METADATA));
        assert!(check_invoice(&pr, MSATS, METADATA, &Currency::Bitcoin).is_err());
    }

    #[test]
    fn rejects_garbage() {
        assert!(check_invoice("lnbc1garbage", MSATS, METADATA, &Currency::Bitcoin).is_err());
    }

    #[tokio::test]
    async fn refuses_redirect_to_http() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            while let Ok((mut socket, _)) = listener.accept().await {
                let mut buf = [0; 1024];
                let n = socket.read(&mut buf).await.unwrap();
                let response = if buf[..n].starts_with(b"GET /start ") {
                    format!("HTTP/1.1 302 Found\r\nLocation: http://{addr}/end\r\nContent-Length: 0\r\n\r\n")
                } else {
                    "HTTP/1.1 200 OK\r\nContent-Length: 0\r\n\r\n".to_string()
                };
                socket.write_all(response.as_bytes()).await.unwrap();
            }
        });

        let result = client()
            .unwrap()
            .get(format!("http://{addr}/start"))
            .send()
            .await;
        assert!(result.is_err());
    }

    #[test]
    fn accepts_https_and_onion() {
        assert!(check_url("https://example.com/cb?k=v").is_ok());
        assert!(check_url("http://abcdef.onion/cb").is_ok());
        assert!(check_url("https://abcdef.onion/cb").is_ok());
    }

    #[test]
    fn rejects_other_urls() {
        assert!(check_url("http://example.com/cb").is_err());
        assert!(check_url("http://onion.example.com/cb").is_err());
        assert!(check_url("ftp://example.com/cb").is_err());
        assert!(check_url("not a url").is_err());
    }
}
