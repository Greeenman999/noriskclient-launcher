use std::{collections::HashMap, path::Path};
use std::path::PathBuf;
use core::option::Option;

use anyhow::Result;
use serde::{Deserialize, Serialize};
use tokio::fs;
use keyring::{Entry as KeyringEntry, Result as KeyringResult};
//use serde::de::Unexpected::Option;

use crate::app::api::LoginData;
use crate::LAUNCHER_DIRECTORY;

fn default_concurrent_downloads() -> i32 {
    10
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub(crate) struct LauncherOptions {
    #[serde(rename = "keepLauncherOpen")]
    pub keep_launcher_open: bool,
    #[serde(rename = "experimentalMode")]
    pub experimental_mode: bool,
    #[serde(rename = "dataPath")]
    pub data_path: String,
    #[serde(rename = "memoryPercentage")]
    pub memory_percentage: i32,
    #[serde(rename = "customJavaPath", default)]
    pub custom_java_path: String,
    #[serde(rename = "customJavaArgs", default)]
    pub custom_java_args: String,
    #[serde(rename = "theme", default)]
    pub theme: String,
    #[serde(rename = "latestBranch")]
    pub latest_branch: Option<String>,
    #[serde(rename = "latestDevBranch")]
    pub latest_dev_branch: Option<String>,
    #[serde(rename = "currentUuid")]
    pub current_uuid: Option<String>,
    #[serde(rename = "accounts")]
    pub accounts: Vec<LoginData>,
    #[serde(rename = "concurrentDownloads", default = "default_concurrent_downloads")]
    pub concurrent_downloads: i32
}

impl LauncherOptions {
    pub async fn load(app_data: &Path) -> Result<Self> {
        // load the options from the file
        let options: LauncherOptions = serde_json::from_slice::<Self>(&fs::read(app_data.join("options.json")).await?)?;

        // load all tokens from keyring
        let service = "noriskclient-launcher";
        let mut accounts = options.accounts.clone();
        for account in &mut accounts {
            let uuid = account.uuid.clone();
            let keyring_mc_token = KeyringEntry::new(service, &*format!("{}-{}", uuid, "mcToken"))?;
            let keyring_access_token = KeyringEntry::new(service, &*format!("{}-{}", uuid, "accessToken"))?;
            let keyring_refresh_token = KeyringEntry::new(service, &*format!("{}-{}", uuid, "refreshToken"))?;
            let keyring_norisk_token = KeyringEntry::new(service, &*format!("{}-{}", uuid, "noriskToken"))?;
            let keyring_experimental_token = KeyringEntry::new(service, &*format!("{}-{}", uuid, "experimentalToken"))?;
            account.mc_token = keyring_mc_token.get_password().unwrap();
            account.access_token = keyring_access_token.get_password().unwrap();
            account.refresh_token = keyring_refresh_token.get_password().unwrap();
            account.norisk_token = keyring_norisk_token.get_password().unwrap();
            account.experimental_token = Some(keyring_experimental_token.get_password().unwrap());
        }

        let mut modified_options = options.clone();
        modified_options.accounts = accounts;

        Ok(modified_options)
    }
    pub async fn store(&self, app_data: &Path) -> Result<()> {
        // store the options in the file
        let accounts = &self.accounts.clone();
        // for each LoginData, store all tokens in keyring
        let service = "noriskclient-launcher";
        for account in accounts {
            let uuid = account.uuid.clone();
            let keyring_mc_token = KeyringEntry::new(service, &*format!("{}-{}", uuid, "mcToken"))?;
            let keyring_access_token = KeyringEntry::new(service, &*format!("{}-{}", uuid, "accessToken"))?;
            let keyring_refresh_token = KeyringEntry::new(service, &*format!("{}-{}", uuid, "refreshToken"))?;
            let keyring_norisk_token = KeyringEntry::new(service, &*format!("{}-{}", uuid, "noriskToken"))?;
            let keyring_experimental_token = KeyringEntry::new(service, &*format!("{}-{}", uuid, "experimentalToken"))?;
            keyring_mc_token.set_password(account.mc_token.clone().as_str()).unwrap();
            keyring_access_token.set_password(account.access_token.clone().as_str()).unwrap();
            keyring_refresh_token.set_password(account.refresh_token.clone().as_str()).unwrap();
            keyring_norisk_token.set_password(account.norisk_token.clone().as_str()).unwrap();
            keyring_experimental_token.set_password(account.experimental_token.clone().unwrap().as_str()).unwrap();
        }

        // remove all tokens from LoginData
        let mut modified_accounts = Vec::new();
        for account in &mut accounts.clone() {
            modified_accounts.push(LoginData {
                uuid: account.uuid.clone(),
                username: account.username.clone(),
                mc_token: String::new(),
                access_token: String::new(),
                refresh_token: String::new(),
                norisk_token: String::new(),
                experimental_token: None::<String>
            });
        }
        let modified_options: LauncherOptions = LauncherOptions {
            keep_launcher_open: self.keep_launcher_open.clone(),
            experimental_mode: self.experimental_mode.clone(),
            data_path: self.data_path.clone(),
            memory_percentage: self.memory_percentage.clone(),
            custom_java_path: self.custom_java_path.clone(),
            custom_java_args: self.custom_java_args.clone(),
            theme: self.theme.clone(),
            latest_branch: self.latest_branch.clone(),
            latest_dev_branch: self.latest_dev_branch.clone(),
            current_uuid: self.current_uuid.clone(),
            accounts: modified_accounts,
            concurrent_downloads: self.concurrent_downloads.clone()
        };

        fs::write(app_data.join("options.json"), serde_json::to_string_pretty(&modified_options)?).await?;
        Ok(())
    }

    pub fn data_path_buf(&self) -> PathBuf {
        if self.data_path.is_empty() {
            return LAUNCHER_DIRECTORY.data_dir().to_path_buf();
        }
        PathBuf::from(&self.data_path)
    }
}

impl Default for LauncherOptions {
    fn default() -> Self {
        let mut theme = "";
        let mode = dark_light::detect();
        match mode {
            // Dark mode
            dark_light::Mode::Dark => {
                theme = "DARK";
            },
            // Light mode
            dark_light::Mode::Light => {
                theme = "LIGHT";
            },
            // Unspecified
            dark_light::Mode::Default => {
                theme = "LIGHT";
            },
        }
        Self {
            keep_launcher_open: true,
            experimental_mode: false,
            data_path: LAUNCHER_DIRECTORY.data_dir().to_str().unwrap().to_string(),
            memory_percentage: 35, // 35% memory of computer allocated to game
            custom_java_path: String::new(),
            custom_java_args: String::new(),
            theme: theme.to_string(),
            latest_branch: None::<String>,
            latest_dev_branch: None::<String>,
            current_uuid: None::<String>,
            accounts: Vec::new(),
            concurrent_downloads: 10
        }
    }
}
