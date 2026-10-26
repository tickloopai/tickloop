use std::{collections::BTreeMap, fs, path::Path};

use serde_json::{Map, Value};
use tick_plugins::{PluginManifest, UserConfigField, UserConfigType};
use tickloop_harness::base::private_fs::{
    PRIVATE_FILE_MODE, PrivateDirectory, replace_file_atomically,
};

use crate::{UserSettings, lock_exclusively, object_entry};

pub const SENSITIVE_PLACEHOLDER: &str = "[敏感配置不在此显示]";

#[derive(Clone, Default, PartialEq, Eq)]
pub struct PluginUserConfig {
    values: BTreeMap<String, Value>,
    sensitive: Vec<String>,
}

impl std::fmt::Debug for PluginUserConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("PluginUserConfig { .. }")
    }
}

impl PluginUserConfig {
    pub fn load(root: &Path, manifest: &PluginManifest) -> Result<Self, String> {
        tick_plugins::validate_plugin_name(&manifest.name).map_err(|error| error.to_string())?;
        let settings = UserSettings::in_root(root)
            .read_object()
            .map_err(|error| error.to_string())?;
        let public = settings
            .get("plugins")
            .and_then(|plugins| plugins.get(&manifest.name))
            .and_then(|plugin| plugin.get("options"));
        let path = root
            .join("credentials")
            .join(&manifest.name)
            .join("user-config.json");
        let private = match fs::read(&path) {
            Ok(bytes) => serde_json::from_slice::<Map<String, Value>>(&bytes).map_err(|_| {
                "cannot read plugin user config: invalid private configuration".to_owned()
            })?,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Map::new(),
            Err(_) => return Err("cannot read private plugin user config".into()),
        };
        let mut config = Self::default();
        for (key, field) in &manifest.user_config {
            if field.sensitive {
                config.sensitive.push(key.clone());
            }
            let saved = if field.sensitive {
                private.get(key)
            } else {
                public.and_then(|values| values.get(key))
            };
            if let Some(value) = saved
                .filter(|value| field.accepts(value))
                .or(field.default.as_ref())
            {
                config.values.insert(key.clone(), value.clone());
            }
        }
        Ok(config)
    }

    pub fn needs_configuration(&self, manifest: &PluginManifest) -> bool {
        manifest
            .user_config
            .iter()
            .any(|(key, field)| field.required && !self.is_filled(key))
            || manifest
                .mcp_servers
                .values()
                .any(|entry| crate::mcp::missing_user_config(entry, &self.variables()))
    }

    pub fn is_filled(&self, key: &str) -> bool {
        self.values.contains_key(key)
    }

    pub fn display_value(&self, key: &str) -> Option<String> {
        self.values.get(key).map(|value| {
            if self.sensitive.iter().any(|sensitive| sensitive == key) {
                "********".into()
            } else {
                value_text(value)
            }
        })
    }

    pub fn secrets(&self) -> Vec<String> {
        self.sensitive
            .iter()
            .filter_map(|key| self.values.get(key))
            .map(value_text)
            .collect()
    }

    pub fn variables(&self) -> BTreeMap<String, String> {
        self.values
            .iter()
            .map(|(key, value)| (format!("user_config.{key}"), value_text(value)))
            .collect()
    }

    pub fn skill_text(&self, markdown: &str) -> String {
        let mut output = String::new();
        let mut rest = markdown;
        while let Some(start) = rest.find("${user_config.") {
            output.push_str(&rest[..start]);
            let variable = &rest[start + "${user_config.".len()..];
            let Some(end) = variable.find('}') else {
                output.push_str(&rest[start..]);
                return output;
            };
            let key = &variable[..end];
            if self.sensitive.iter().any(|sensitive| sensitive == key) {
                output.push_str(SENSITIVE_PLACEHOLDER);
            } else if let Some(value) = self.values.get(key) {
                output.push_str(&value_text(value));
            } else {
                output.push_str("[configuration required]");
            }
            rest = &variable[end + 1..];
        }
        output.push_str(rest);
        output
    }

    pub fn save(
        &mut self,
        root: &Path,
        manifest: &PluginManifest,
        updates: &BTreeMap<String, Value>,
    ) -> Result<(), String> {
        tick_plugins::validate_plugin_name(&manifest.name).map_err(|error| error.to_string())?;
        for (key, value) in updates {
            if !manifest
                .user_config
                .get(key)
                .is_some_and(|field| field.accepts(value))
            {
                return Err("invalid plugin configuration value".into());
            }
        }
        let directory = PrivateDirectory::anchored(
            root.to_owned(),
            Path::new("credentials").join(&manifest.name),
        );
        directory
            .create()
            .map_err(|_| "cannot create private plugin configuration directory".to_owned())?;
        let _lock = lock_exclusively(&directory.path().join("user-config.lock"))
            .map_err(|_| "cannot lock plugin configuration".to_owned())?;
        let path = directory.path().join("user-config.json");
        let mut private = match fs::read(&path) {
            Ok(bytes) => serde_json::from_slice::<Map<String, Value>>(&bytes)
                .map_err(|_| "invalid private plugin configuration".to_owned())?,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Map::new(),
            Err(_) => return Err("cannot read private plugin configuration".into()),
        };
        for (key, value) in updates {
            if manifest.user_config[key].sensitive {
                private.insert(key.clone(), value.clone());
            }
        }
        replace_file_atomically(
            &path,
            &serde_json::to_vec_pretty(&private).expect("JSON serializes"),
            PRIVATE_FILE_MODE,
        )
        .map_err(|_| "cannot save private plugin configuration".to_owned())?;
        UserSettings::in_root(root)
            .update(|settings| {
                let plugins = object_entry(settings, "plugins", "plugins".into())?;
                let plugin = object_entry(plugins, &manifest.name, "plugins entry".into())?;
                let options = object_entry(plugin, "options", "plugin options".into())?;
                for (key, field) in &manifest.user_config {
                    if field.sensitive {
                        options.remove(key);
                    } else if let Some(value) = updates.get(key) {
                        options.insert(key.clone(), value.clone());
                    }
                }
                Ok(())
            })
            .map_err(|error| error.to_string())?;
        *self = Self::load(root, manifest)?;
        Ok(())
    }
}

pub fn parse_input(field: &UserConfigField, text: &str) -> Result<Value, &'static str> {
    let value = match field.kind {
        UserConfigType::String | UserConfigType::Directory | UserConfigType::File => {
            Value::String(text.into())
        }
        UserConfigType::Number | UserConfigType::Boolean => {
            serde_json::from_str(text).map_err(|_| "Enter a valid value of the declared type")?
        }
    };
    if field.accepts(&value) {
        Ok(value)
    } else {
        Err("Enter a valid value of the declared type")
    }
}

fn value_text(value: &Value) -> String {
    value
        .as_str()
        .map(str::to_owned)
        .unwrap_or_else(|| value.to_string())
}
