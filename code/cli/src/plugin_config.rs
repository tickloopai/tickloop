use std::{
    collections::BTreeMap,
    io::{self, BufRead, IsTerminal, Write},
};
use tick_plugins::{InstalledPlugin, PluginStore};
use tickloop_code_console::terminal::safe_line;
use tickloop_code_settings::plugin_config::{PluginUserConfig, parse_input};

pub(super) fn configure(
    store: &PluginStore,
    plugin: &InstalledPlugin,
    all: bool,
) -> Result<(), String> {
    let mut config = PluginUserConfig::load(store.config_root(), &plugin.manifest)?;
    let fields = plugin
        .manifest
        .user_config
        .iter()
        .filter(|(key, _)| all || !config.is_filled(key))
        .collect::<Vec<_>>();
    if fields.is_empty() {
        return Ok(());
    }
    if !io::stdin().is_terminal() || !io::stdout().is_terminal() {
        let hint = format!(
            "Open a terminal and run: tickloop plugin configure {}",
            plugin.name()
        );
        if all {
            return Err(hint);
        }
        println!("Configuration needed. {hint}");
        return Ok(());
    }
    let mut updates = BTreeMap::new();
    for (key, field) in fields {
        let title = safe_line(field.title.as_deref().unwrap_or(key));
        if let Some(description) = &field.description {
            println!("{}", safe_line(description));
        }
        loop {
            let saved = config
                .display_value(key)
                .map(|value| format!(" [Enter keeps {}]", safe_line(&value)))
                .unwrap_or_default();
            print!(
                "{title}{saved}{}: ",
                if field.sensitive {
                    " (hidden as you type)"
                } else {
                    ""
                }
            );
            io::stdout().flush().map_err(|error| error.to_string())?;
            let entered = if field.sensitive {
                let value =
                    super::plugin_tick::read_hidden_line().map_err(|error| error.to_string())?;
                println!();
                value.ok_or("Plugin configuration cancelled")?
            } else {
                let mut value = String::new();
                if io::stdin()
                    .lock()
                    .read_line(&mut value)
                    .map_err(|error| error.to_string())?
                    == 0
                {
                    return Err("Plugin configuration cancelled".into());
                }
                value.trim_end_matches(['\r', '\n']).to_owned()
            };
            if entered.is_empty() && (config.is_filled(key) || !field.required) {
                break;
            }
            match parse_input(field, &entered) {
                Ok(value) => {
                    updates.insert(key.clone(), value);
                    break;
                }
                Err(error) => println!("{error}"),
            }
        }
    }
    config.save(store.config_root(), &plugin.manifest, &updates)
}
