use std::{collections::HashMap, fmt, marker::PhantomData, path::{Path, PathBuf}, str::FromStr};

use anyhow::Result;
use tracing::{debug, info};
use tokio::fs;
use serde::{Deserialize, Deserializer, de::{self, MapAccess, Visitor}};
use void::Void;
use std::collections::HashSet;
use crate::{error::LauncherError, HTTP_CLIENT, LAUNCHER_DIRECTORY, utils::{download_file_untracked, Architecture}};
use crate::utils::{get_maven_artifact_path, sha1sum};
use std::sync::Arc;
use crate::app::api::get_launcher_api_base;
use crate::app::app_data::LauncherOptions;
use crate::minecraft::launcher::LaunchingParameter;
use crate::minecraft::progress::{ProgressReceiver, ProgressUpdate};

// https://launchermeta.mojang.com/mc/game/version_manifest.json

#[derive(Deserialize)]
pub struct VersionManifest {
    pub versions: Vec<ManifestVersion>,
}

impl VersionManifest {
    pub async fn download() -> Result<Self> {
        let response = HTTP_CLIENT.get("https://launchermeta.mojang.com/mc/game/version_manifest.json")
            .send().await?
            .error_for_status()?;
        let manifest = response.json::<VersionManifest>().await?;

        Ok(manifest)
    }
}

#[derive(Deserialize)]
pub struct ManifestVersion {
    pub id: String,
    #[serde(rename = "type")]
    pub version_type: String,
    pub url: String,
    pub time: String,
    #[serde(rename = "releaseTime")]
    pub release_time: String,
}

#[derive(Deserialize)]
pub struct VersionProfile {
    pub id: String,
    #[serde(rename = "assetIndex")]
    pub asset_index_location: Option<AssetIndexLocation>,
    pub assets: Option<String>,
    #[serde(rename = "inheritsFrom")]
    pub inherits_from: Option<String>,
    #[serde(rename = "minimumLauncherVersion")]
    pub minimum_launcher_version: Option<i32>,
    pub downloads: Option<Downloads>,
    #[serde(rename = "complianceLevel")]
    pub compliance_level: Option<i32>,
    pub libraries: Vec<Library>,
    #[serde(rename = "mainClass")]
    pub main_class: Option<String>,
    pub logging: Option<Logging>,
    #[serde(rename = "type")]
    pub version_type: String,
    #[serde(flatten)]
    pub arguments: ArgumentDeclaration,
}

impl VersionProfile {
    pub(crate) fn merge(&mut self, mut parent: VersionProfile) -> Result<()> {
        Self::merge_options(&mut self.asset_index_location, parent.asset_index_location);
        Self::merge_options(&mut self.assets, parent.assets);

        Self::merge_larger(&mut self.minimum_launcher_version, parent.minimum_launcher_version);
        Self::merge_options(&mut self.downloads, parent.downloads);
        Self::merge_larger(&mut self.compliance_level, parent.compliance_level);

        self.libraries.append(&mut parent.libraries);
        Self::merge_options(&mut self.main_class, parent.main_class);
        Self::merge_options(&mut self.logging, parent.logging);

        match &mut self.arguments {
            ArgumentDeclaration::V14(v14_a) => {
                if let ArgumentDeclaration::V14(v14_b) = parent.arguments {
                    Self::merge_options(&mut v14_a.minecraft_arguments, v14_b.minecraft_arguments);
                } else {
                    return Err(LauncherError::InvalidVersionProfile("version profile inherits from incompatible profile".to_string()).into());
                }
            }
            ArgumentDeclaration::V21(v21_a) => {
                if let ArgumentDeclaration::V21(mut v21_b) = parent.arguments {
                    v21_a.arguments.game.append(&mut v21_b.arguments.game);
                    v21_a.arguments.jvm.append(&mut v21_b.arguments.jvm);
                } else {
                    return Err(LauncherError::InvalidVersionProfile("version profile inherits from incompatible profile".to_string()).into());
                }
            }
        }

        Ok(())
    }

    fn merge_options<T>(a: &mut Option<T>, b: Option<T>) {
        if !a.is_some() {
            *a = b;
        }
    }

    fn merge_larger<T>(a: &mut Option<T>, b: Option<T>) where T: Ord {
        if let Some((val_a, val_b)) = a.as_ref().zip(b.as_ref()) {
            if val_a < val_b {
                *a = b;
            }
        } else if !a.is_some() {
            *a = b;
        }
    }
}

#[derive(Deserialize)]
#[serde(untagged)] // TODO: Might guess from minimum_launcher_version just to be sure.
pub enum ArgumentDeclaration {
    /// V21 describes the new version json used by versions above 1.13.
    V21(V21ArgumentDeclaration),
    /// V14 describes the old version json used by versions below 1.12.2
    V14(V14ArgumentDeclaration),
}

impl ArgumentDeclaration {
    pub(crate) fn add_jvm_args_to_vec(&self, norisk_token: &str, command_arguments: &mut Vec<String>, parameter: &LaunchingParameter, features: &HashSet<String>) -> Result<()> {
        command_arguments.push(format!("-Xmx{}M", parameter.memory));
        command_arguments.push("-XX:+UnlockExperimentalVMOptions".to_string());
        command_arguments.push("-XX:+UseG1GC".to_string());
        command_arguments.push("-XX:G1NewSizePercent=20".to_string());
        command_arguments.push("-XX:G1ReservePercent=20".to_string());
        command_arguments.push("-XX:MaxGCPauseMillis=50".to_string());
        command_arguments.push("-XX:G1HeapRegionSize=32M".to_string());
        command_arguments.push(format!("-Dnorisk.token={}", norisk_token));
        command_arguments.push(format!("-Dnorisk.experimental={}", parameter.dev_mode));
        for arg in parameter.custom_java_args.split(" ") {
            if arg != " " && arg != "" {
                println!("Added custom java arg: {:?}", arg);
                command_arguments.push(arg.to_string());
            }
        }

        match self {
            ArgumentDeclaration::V14(_) => command_arguments.append(&mut vec!["-Djava.library.path=${natives_directory}".to_string(), "-cp".to_string(), "${classpath}".to_string()]),
            ArgumentDeclaration::V21(decl) => {
                ArgumentDeclaration::check_rules_and_add(command_arguments, &decl.arguments.jvm, features)?;
            }
        }

        Ok(())
    }
    pub(crate) fn add_game_args_to_vec(&self, command_arguments: &mut Vec<String>, features: &HashSet<String>) -> Result<()> {
        match self {
            ArgumentDeclaration::V14(decl) => {
                command_arguments.extend(
                    decl.minecraft_arguments
                        .as_ref()
                        .ok_or_else(|| LauncherError::InvalidVersionProfile("no game arguments specified".to_string()))?
                        .split(" ")
                        .map(ToOwned::to_owned)
                );
            }
            ArgumentDeclaration::V21(decl) => {
                ArgumentDeclaration::check_rules_and_add(command_arguments, &decl.arguments.game, features)?;
            }
        }

        Ok(())
    }

    fn check_rules_and_add(command_arguments: &mut Vec<String>, args: &Vec<Argument>, features: &HashSet<String>) -> Result<()> {
        for argument in args {
            if let Some(rules) = &argument.rules {
                if !crate::minecraft::rule_interpreter::check_condition(rules, &features)? {
                    continue;
                }
            }

            match &argument.value {
                ArgumentValue::SINGLE(value) => command_arguments.push(value.to_owned()),
                ArgumentValue::VEC(vec) => command_arguments.append(&mut vec.clone())
            };
        }

        Ok(())
    }
}

#[derive(Deserialize)]
pub struct V14ArgumentDeclaration {
    #[serde(rename = "minecraftArguments")]
    pub minecraft_arguments: Option<String>,
}

#[derive(Deserialize)]
pub struct V21ArgumentDeclaration {
    pub arguments: Arguments,
}

impl VersionProfile {
    pub async fn load(url: &String) -> Result<Self> {
        dbg!(url);
        Ok(HTTP_CLIENT.get(url).send().await?.error_for_status()?.json::<VersionProfile>().await?)
    }
}

// Parsing the arguments was pain, please mojang. What in the hell did you do?
// https://github.com/serde-rs/serde/issues/723 That's why I've done a workaround using vec_argument

#[derive(Deserialize)]
pub struct Arguments {
    #[serde(default)]
    #[serde(deserialize_with = "vec_argument")]
    pub game: Vec<Argument>,
    #[serde(default)]
    #[serde(deserialize_with = "vec_argument")]
    pub jvm: Vec<Argument>,
}

#[derive(Deserialize)]
pub struct Argument {
    pub rules: Option<Vec<Rule>>,
    pub value: ArgumentValue,
}

#[derive(Deserialize)]
#[serde(untagged)]
pub enum ArgumentValue {
    SINGLE(String),
    VEC(Vec<String>),
}

impl FromStr for Argument {
    type Err = Void;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Ok(Argument { value: ArgumentValue::SINGLE(s.to_string()), rules: None })
    }
}


fn vec_argument<'de, D>(deserializer: D) -> Result<Vec<Argument>, D::Error>
    where
        D: Deserializer<'de>,
{
    #[derive(Deserialize)]
    struct Wrapper(#[serde(deserialize_with = "string_or_struct")] Argument);

    let v = Vec::deserialize(deserializer).unwrap();
    Ok(v.into_iter().map(|Wrapper(a)| a).collect())
}

fn string_or_struct<'de, T, D>(deserializer: D) -> Result<T, D::Error>
    where
        T: Deserialize<'de> + FromStr<Err=Void>,
        D: Deserializer<'de>,
{
    struct StringOrStruct<T>(PhantomData<fn() -> T>);

    impl<'de, T> Visitor<'de> for StringOrStruct<T>
        where
            T: Deserialize<'de> + FromStr<Err=Void>,
    {
        type Value = T;

        fn expecting(&self, formatter: &mut fmt::Formatter) -> fmt::Result {
            formatter.write_str("string or map")
        }

        fn visit_str<E>(self, value: &str) -> Result<T, E>
            where
                E: serde::de::Error,
        {
            Ok(FromStr::from_str(value).unwrap())
        }

        fn visit_map<M>(self, map: M) -> Result<T, M::Error>
            where
                M: MapAccess<'de>,
        {
            Deserialize::deserialize(de::value::MapAccessDeserializer::new(map))
        }
    }

    deserializer.deserialize_any(StringOrStruct(PhantomData))
}

#[derive(Deserialize)]
pub struct AssetIndexLocation {
    pub id: String,
    pub sha1: String,
    pub size: i64,
    #[serde(rename = "totalSize")]
    pub total_size: i64,
    pub url: String,
}

impl AssetIndexLocation {
    pub async fn load_asset_index(&self, assets_root: &PathBuf) -> Result<AssetIndex> {
        let asset_index = assets_root.join(format!("{}.json", &self.id));

        if !asset_index.exists() {
            info!("Downloading assets index of {}", self.id);
            download_file_untracked(&self.url, &asset_index).await?;
            info!("Downloaded {}", self.url);
        }

        let content = &*fs::read(&asset_index).await?;
        Ok(serde_json::from_slice::<AssetIndex>(content)?)
    }
}

#[derive(Deserialize)]
pub struct AssetIndex {
    pub objects: HashMap<String, AssetObject>,
}

#[derive(Deserialize, Clone)]
pub struct AssetObject {
    pub hash: String,
    pub size: i64,
}

impl AssetObject {
    pub async fn download(&self, assets_objects_folder: impl AsRef<Path>, progress: Arc<impl ProgressReceiver>) -> Result<bool> {
        let assets_objects_folder = assets_objects_folder.as_ref().to_owned();
        let asset_folder = assets_objects_folder.join(&self.hash[0..2]);

        if !asset_folder.exists() {
            fs::create_dir(&asset_folder).await?;
        }

        let asset_path = asset_folder.join(&self.hash);

        return if !asset_path.exists() {
            progress.progress_update(ProgressUpdate::set_label(format!("Downloading asset object {}", self.hash)));

            info!("Downloading {}", self.hash);
            download_file_untracked(&*format!("https://resources.download.minecraft.net/{}/{}", &self.hash[0..2], &self.hash), asset_path).await?;
            info!("Downloaded {}", self.hash);

            Ok(true)
        } else {
            Ok(false)
        };
    }

    pub async fn download_norisk_cosmetic(&self, branch: String, file_path: String, assets_objects_folder: impl AsRef<Path>, progress: Arc<impl ProgressReceiver>) -> Result<bool> {
        let options = LauncherOptions::load(LAUNCHER_DIRECTORY.config_dir()).await.unwrap_or_default();
        let assets_objects_folder = assets_objects_folder.as_ref().to_owned();

        let mut path_parts: Vec<&str> = file_path.split("/").collect();

        let mut asset_file_path = assets_objects_folder.clone();
        for part in path_parts.clone() {
            asset_file_path = asset_file_path.join(part);
        }

        path_parts.pop();

        let mut asset_path = assets_objects_folder.clone();
        for part in path_parts {
            asset_path = asset_path.join(part);
        }

        if !asset_path.exists() {
            fs::create_dir_all(&asset_path).await?;
        }

        let mut download = false;

        if (asset_file_path.exists()) {
            let sha1 = sha1sum(&asset_file_path)?;

            if &self.hash == &sha1 {
                // If sha1 matches, return
                info!("Norisk asset {} already exists and matches sha1.", &self.hash);
            } else {
                info!("Norisk asset {} already exists but does not match sha1.", &self.hash);
                download = true;
            }
        } else {
            download = true;
        }

        return if download {
            progress.progress_update(ProgressUpdate::set_label(format!("Downloading asset object {}", self.hash)));

            info!("Downloading {}", self.hash);
            download_file_untracked(&*format!("{}/launcherapi/v1/assets/{}/{}/{}", get_launcher_api_base(options.experimental_mode), branch, &self.hash[0..2], &self.hash), asset_file_path).await?;
            info!("Downloaded {}", self.hash);

            Ok(true)
        } else {
            Ok(false)
        };
    }

    pub async fn download_destructing(self, assets_objects_folder: impl AsRef<Path>, progress: Arc<impl ProgressReceiver>) -> Result<bool> {
        return self.download(assets_objects_folder, progress).await;
    }

    pub async fn download_norisk_cosmetic_destructing(self, branch: String, file_path: String, assets_objects_folder: impl AsRef<Path>, progress: Arc<impl ProgressReceiver>) -> Result<bool> {
        return self.download_norisk_cosmetic(branch, file_path, assets_objects_folder, progress).await;
    }
}

#[derive(Deserialize)]
pub struct Downloads {
    pub client: Option<Download>,
    pub client_mappings: Option<Download>,
    pub server: Option<Download>,
    pub server_mappings: Option<Download>,
    pub windows_server: Option<Download>,
}


#[derive(Deserialize)]
pub struct Download {
    pub sha1: String,
    pub size: i64,
    pub url: String,
}

impl Download {
    pub async fn download(&self, path: impl AsRef<Path>) -> Result<()> {
        download_file_untracked(&self.url, path).await?;
        info!("Downloaded {}", self.url);
        Ok(())
    }
}

#[derive(Deserialize, Clone)]
pub struct Library {
    pub name: String,
    pub downloads: Option<LibraryDownloads>,
    pub natives: Option<HashMap<String, String>>,
    #[serde(default)]
    pub rules: Vec<Rule>,
    pub url: Option<String>,
}

impl Library {
    pub fn get_library_download(&self) -> Result<LibraryDownloadInfo> {
        if let Some(artifact) = self.downloads.as_ref().and_then(|x| x.artifact.as_ref()) {
            return Ok(artifact.into());
        }

        let path = get_maven_artifact_path(&self.name)?;
        let url = self.url.as_deref().unwrap_or("https://libraries.minecraft.net/");

        return Ok(
            LibraryDownloadInfo {
                url: format!("{}{}", url, path),
                sha1: None,
                size: None,
                path,
            }
        );
    }
}

#[derive(Deserialize, Clone)]
pub struct Rule {
    pub action: RuleAction,
    pub os: Option<OsRule>,
    pub features: Option<HashMap<String, bool>>,
}

#[derive(Deserialize, Clone)]
pub struct OsRule {
    pub name: Option<String>,
    pub version: Option<String>,
    pub arch: Option<Architecture>,
}

#[derive(Deserialize, Clone)]
pub enum RuleAction {
    #[serde(rename = "allow")]
    Allow,
    #[serde(rename = "disallow")]
    Disallow,
}

#[derive(Deserialize, Clone)]
pub struct LibraryDownloads {
    pub artifact: Option<LibraryArtifact>,
    pub classifiers: Option<HashMap<String, LibraryArtifact>>,
}

#[derive(Deserialize, Clone)]
pub struct LibraryArtifact {
    pub path: String,
    pub sha1: String,
    pub size: i64,
    pub url: String,
}

#[derive(Deserialize, Clone)]
pub struct LibraryDownloadInfo {
    pub path: String,
    pub sha1: Option<String>,
    pub size: Option<i64>,
    pub url: String,
}

impl From<&LibraryArtifact> for LibraryDownloadInfo {
    fn from(artifact: &LibraryArtifact) -> Self {
        LibraryDownloadInfo {
            path: artifact.path.to_owned(),
            sha1: Some(artifact.sha1.to_owned()),
            size: Some(artifact.size),
            url: artifact.url.to_owned(),
        }
    }
}

impl LibraryDownloadInfo {
    async fn fetch_sha1(&self) -> Result<String> {
        HTTP_CLIENT.get(&format!("{}{}", &self.url, ".sha1"))
            .send().await?
            .error_for_status()?
            .text()
            .await
            .map_err(|e| anyhow::anyhow!(e))
    }

    pub async fn download(&self, name: String, libraries_folder: &Path, progress: Arc<impl ProgressReceiver>) -> Result<PathBuf> {
        info!("Downloading library {}, sha1: {:?}, size: {:?}", name, &self.sha1, &self.size);
        debug!("Library download url: {}", &self.url);

        let path = libraries_folder.to_path_buf();
        let library_path = path.join(&self.path);

        // Create parent directories
        fs::create_dir_all(&library_path.parent().unwrap()).await?;

        // SHA1
        let sha1 = if let Some(sha1) = &self.sha1 {
            Some(sha1.clone())
        } else {
            // Check if sha1 file exists
            let sha1_path = path.join(&self.path).with_extension("sha1");

            if sha1_path.exists() {
                // If sha1 file exists, read it
                let sha1 = fs::read_to_string(&sha1_path).await?;
                Some(sha1)
            } else {
                // If sha1 file doesn't exist, fetch it
                let sha1 = self.fetch_sha1().await
                    .map(Some)
                    .unwrap_or(None);

                // Write sha1 file
                if let Some(sha1) = &sha1 {
                    fs::write(&sha1_path, &sha1).await?;
                }

                sha1
            }
        };

        // Check if library already exists
        if library_path.exists() {
            // Check if sha1 matches
            let hash = sha1sum(&library_path)?;

            if let Some(sha1) = &sha1 {
                if hash == *sha1 {
                    // If sha1 matches, return
                    info!("Library {} already exists and matches sha1.", name);
                    return Ok(library_path);
                }
            } else {
                // If sha1 is not available, assume it matches
                info!("Library {} already exists.", name);
                return Ok(library_path);
            }

            // If sha1 doesn't match, remove the file
            info!("Library {} already exists but sha1 doesn't match, redownloading", name);
            fs::remove_file(&library_path).await?;
        }

        // Download library
        progress.progress_update(ProgressUpdate::set_label(format!("Downloading library {}", name)));

        download_file_untracked(&self.url, &library_path).await?;
        info!("Downloaded {}", self.url);

        // After downloading, check sha1
        if let Some(sha1) = &sha1 {
            let hash = sha1sum(&library_path)?;
            if hash != *sha1 {
                anyhow::bail!("sha1 of downloaded library {} doesn't match", name);
            }
        }

        Ok(library_path)
    }
}

#[derive(Deserialize)]
pub struct Logging {
    // TODO: Add logging configuration
}
