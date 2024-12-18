#![allow(clippy::blocks_in_conditions)] // TODO: `rustc` 1.80.1 clippy issue
mod cache_types;

use async_trait::async_trait;
use derive_more::Deref;
use derive_new::new;
use ethers::{abi::AbiDecode, core::utils::hex::decode as hex_decode};
use eyre::Context;
use regex::Regex;
use reqwest::Client;
use serde::{Deserialize, Serialize};
use serde_json::json;
use tracing::{info, instrument};

use hyperlane_core::{
    unwrap_or_none_result, utils::bytes_to_hex, CcipReadIsm, HyperlaneMessage, RawHyperlaneMessage,
    H256,
};
use hyperlane_ethereum::OffchainLookup;

use cache_types::SerializedOffchainLookup;

use super::{base::MessageMetadataBuilder, MetadataBuilder};

#[derive(Serialize, Deserialize)]
struct OffchainResponse {
    data: String,
}

#[derive(Clone, Debug, new, Deref)]
pub struct CcipReadIsmMetadataBuilder {
    base: MessageMetadataBuilder,
}

impl CcipReadIsmMetadataBuilder {
    /// Returns info on how to query for offchain information
    /// This method will attempt to get the value from cache first. If it is a cache miss,
    /// it will request it from the ISM contract. The result will be cached for future use.
    ///
    /// Implicit contract in this method: function name `get_offchain_verify_info` matches
    /// the name of the method `get_offchain_verify_info`.
    async fn call_get_offchain_verify_info(
        &self,
        ism: &dyn CcipReadIsm,
        message: &HyperlaneMessage,
    ) -> eyre::Result<Option<OffchainLookup>> {
        let contract_address = Some(ism.address());
        let fn_name = "get_offchain_verify_info";
        let parsed_message = RawHyperlaneMessage::from(message).to_vec();

        let info_from_cache = self
            .get_cached_call_result::<SerializedOffchainLookup>(
                contract_address,
                fn_name,
                &parsed_message,
            )
            .await;

        let info: OffchainLookup = match info_from_cache {
            Some(info) => info.into(),
            None => {
                let response = ism.get_offchain_verify_info(parsed_message.clone()).await;
                match response {
                    Ok(_) => {
                        info!("incorrectly configured getOffchainVerifyInfo, expected revert");
                        return Ok(None);
                    }
                    Err(raw_error) => {
                        let matching_regex = Regex::new(r"0x[[:xdigit:]]+")?;
                        if let Some(matching) = &matching_regex.captures(&raw_error.to_string()) {
                            OffchainLookup::decode(hex_decode(&matching[0][2..])?)?
                        } else {
                            info!("unable to parse custom error out of revert");
                            return Ok(None);
                        }
                    }
                }
            }
        };

        self.cache_call_result(
            contract_address,
            fn_name,
            &parsed_message,
            &SerializedOffchainLookup::from(info.clone()),
        )
        .await;

        Ok(Some(info))
    }
}

#[async_trait]
impl MetadataBuilder for CcipReadIsmMetadataBuilder {
    #[instrument(err, skip(self))]
    async fn build(
        &self,
        ism_address: H256,
        message: &HyperlaneMessage,
    ) -> eyre::Result<Option<Vec<u8>>> {
        const CTX: &str = "When fetching CcipRead metadata";
        let ism = self.build_ccip_read_ism(ism_address).await.context(CTX)?;

        let info: OffchainLookup =
            unwrap_or_none_result!(self.call_get_offchain_verify_info(&ism, message).await?);

        for url in info.urls.iter() {
            // Need to explicitly convert the sender H160 the hex because the `ToString` implementation
            // for `H160` truncates the output. (e.g. `0xc66a…7b6f` instead of returning
            // the full address)
            let sender_as_bytes = &bytes_to_hex(info.sender.as_bytes());
            let data_as_bytes = &info.call_data.to_string();
            let interpolated_url = url
                .replace("{sender}", sender_as_bytes)
                .replace("{data}", data_as_bytes);
            let res = if !url.contains("{data}") {
                let body = json!({
                    "sender": sender_as_bytes,
                    "data": data_as_bytes
                });
                Client::new()
                    .post(interpolated_url)
                    .header("Content-Type", "application/json")
                    .json(&body)
                    .send()
                    .await?
            } else {
                reqwest::get(interpolated_url).await?
            };

            let json: Result<OffchainResponse, reqwest::Error> = res.json().await;

            match json {
                Ok(result) => {
                    // remove leading 0x which hex_decode doesn't like
                    let metadata = hex_decode(&result.data[2..])?;
                    return Ok(Some(metadata));
                }
                Err(_err) => {
                    // try the next URL
                }
            }
        }

        // No metadata endpoints or endpoints down
        Ok(None)
    }
}