use std::{
    sync::{Arc, Mutex},
    time::Duration,
};

use anyhow::bail;
use log::{debug, error, info, warn};
use reqwest::StatusCode;
use reqwest_middleware::ClientWithMiddleware as Client;
use serde::Deserialize;

use crate::{
    disk::state::ManualDonationLevel,
    helper::{xvb::output_console, Process, ProcessName, ProcessState},
    XVB_URL,
};
use crate::{
    disk::state::XvbMode, XVB_ROUND_DONOR_MEGA_MIN_HR, XVB_ROUND_DONOR_MIN_HR,
    XVB_ROUND_DONOR_VIP_MIN_HR, XVB_ROUND_DONOR_WHALE_MIN_HR,
};

use super::{nodes::XvbNode, rounds::XvbRound, PubXvbApi};

#[derive(Debug, Clone, Deserialize, PartialEq, Eq, Default)]
pub enum RuntimeMode {
    #[default]
    Auto,
    ManualXvb,
    ManualP2pool,
    Hero,
    ManualDonationLevel,
}

#[derive(Debug, Clone, Deserialize, PartialEq, Eq, Default)]
pub enum RuntimeDonationLevel {
    #[default]
    Donor,
    DonorVIP,
    DonorWhale,
    DonorMega,
}

impl RuntimeDonationLevel {
    pub fn get_hashrate(&self) -> f32 {
        match &self {
            Self::Donor => XVB_ROUND_DONOR_MIN_HR as f32,
            Self::DonorVIP => XVB_ROUND_DONOR_VIP_MIN_HR as f32,
            Self::DonorWhale => XVB_ROUND_DONOR_WHALE_MIN_HR as f32,
            Self::DonorMega => XVB_ROUND_DONOR_MEGA_MIN_HR as f32,
        }
    }
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct XvbPrivStats {
    pub fails: u8,
    pub donor_1hr_avg: f32,
    pub donor_24hr_avg: f32,
    #[serde(skip)]
    pub win_current: bool,
    #[serde(skip)]
    pub round_participate: Option<XvbRound>,
    #[serde(skip)]
    pub node: XvbNode,
    #[serde(skip)]
    // it is the time remaining before switching from P2pool to XvB or XvB to P2ool.
    // it is not the time remaining of the algo, even if it could be the same if never mining on XvB.
    pub time_switch_node: u32,
    #[serde(skip)]
    pub msg_indicator: String,
    #[serde(skip)]
    // so the hero mode can change between two decision of algorithm without restarting XvB.
    pub runtime_mode: RuntimeMode,
    #[serde(skip)]
    pub runtime_manual_amount: f64,
    #[serde(skip)]
    pub runtime_manual_donation_level: RuntimeDonationLevel,
}

impl XvbPrivStats {
    pub async fn request_api(client: &Client, address: &str, token: &str) -> anyhow::Result<Self> {
        let resp = client
            .get(
                [
                    XVB_URL,
                    "/cgi-bin/p2pool_bonus_history_gupaxx_api.cgi?address=",
                    address,
                    "&token=",
                    token,
                ]
                .concat(),
            )
            .timeout(Duration::from_secs(10))
            .send()
            .await?;
        match resp.status() {
            StatusCode::OK => match resp.json::<Self>().await {
                Ok(s) => Ok(s),
                Err(err) => {
                    error!("XvB Watchdog | Data provided from private API is not deserializ-able.Error: {}", err);
                    bail!(
                        "Data provided from private API is not deserializ-able.Error: {}",
                        err
                    );
                }
            },
            StatusCode::UNPROCESSABLE_ENTITY => {
                bail!("the token is invalid for this xmr address.")
            }
            _ => bail!("The status of the response is not expected"),
        }
    }
    pub async fn update_stats(
        client: &Client,
        address: &str,
        token: &str,
        pub_api: &Arc<Mutex<PubXvbApi>>,
        gui_api: &Arc<Mutex<PubXvbApi>>,
        process: &Arc<Mutex<Process>>,
    ) {
        match XvbPrivStats::request_api(client, address, token).await {
            Ok(new_data) => {
                debug!("XvB Watchdog | HTTP API request OK");
                pub_api.lock().unwrap().stats_priv = new_data;
                let previously_failed = process.lock().unwrap().state == ProcessState::Failed;
                if previously_failed {
                    info!("XvB Watchdog | Public stats are working again");
                    output_console(
                        &mut gui_api.lock().unwrap().output,
                        "requests for public API are now working",
                        ProcessName::Xvb,
                    );
                    process.lock().unwrap().state = ProcessState::Syncing;
                }
                // if last request failed, we are now ready to show stats again and maybe be alive next loop.
            }
            Err(err) => {
                warn!(
                    "XvB Watchdog | Could not send HTTP private API request to: {}\n:{}",
                    XVB_URL, err
                );
                if process.lock().unwrap().state != ProcessState::Failed {
                    output_console(
                        &mut gui_api.lock().unwrap().output,
                        "Failure to retrieve private stats \nWill retry shortly...",
                        ProcessName::Xvb,
                    );
                }
                // we stop the algo (will be stopped by the check status on next loop) because we can't make the rest work without public stats. (winner in xvb private stats).
                output_console(
                    &mut gui_api.lock().unwrap().output,
                    "request to get private API failed",
                    ProcessName::Xvb,
                );
                process.lock().unwrap().state = ProcessState::Failed;
            }
        }
    }
}

impl From<XvbMode> for RuntimeMode {
    fn from(mode: XvbMode) -> Self {
        match mode {
            XvbMode::Auto => Self::Auto,
            XvbMode::ManualXvb => Self::ManualXvb,
            XvbMode::ManualP2pool => Self::ManualP2pool,
            XvbMode::Hero => Self::Hero,
            XvbMode::ManualDonationLevel => Self::ManualDonationLevel,
        }
    }
}

impl From<ManualDonationLevel> for RuntimeDonationLevel {
    fn from(level: ManualDonationLevel) -> Self {
        match level {
            ManualDonationLevel::Donor => RuntimeDonationLevel::Donor,
            ManualDonationLevel::DonorVIP => RuntimeDonationLevel::DonorVIP,
            ManualDonationLevel::DonorWhale => RuntimeDonationLevel::DonorWhale,
            ManualDonationLevel::DonorMega => RuntimeDonationLevel::DonorMega,
        }
    }
}
