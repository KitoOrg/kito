//! A server's plugins, changed from somewhere else than its own machine
//! (ADR 0008, section 7): what `/-/admin/plugins` does for the people the
//! server's machine marked as administrators.
//!
//! Only by name, only from the registry the server was told: no address,
//! no path, no upload. Nothing is enabled by installing, and what is
//! given to a plugin is said by whoever enables it.

use crate::state::{Server, ENV_SERVER_DIR};
use kitod::plugins::{self, Permission};
use serde::Deserialize;
use serde_json::{json, Value};

/// What an administrator asks: `POST /-/admin/plugins`.
#[derive(Deserialize)]
pub struct Ask {
    pub action: String,
    pub name: String,
    /// `install`: the version, or the latest that fits.
    #[serde(default)]
    pub version: Option<String>,
    /// `enable`: what the plugin is given; none said: what it asks for.
    #[serde(default)]
    pub permissions: Option<Vec<Permission>>,
    /// `enable`: it runs in the directories connected to the server too.
    #[serde(default)]
    pub for_clients: bool,
    /// `disable`: what the plugin kept on the server goes too.
    #[serde(default)]
    pub purge: bool,
}

/// What a plugin adds to a server, in words.
fn adds(manifest: &plugins::Manifest) -> Vec<String> {
    let mut adds = Vec::new();
    if let Some(sign_in) = &manifest.sign_in {
        adds.push(format!("signing in with {}", sign_in.method));
    }
    if let Some(pages) = &manifest.pages {
        adds.push(format!("pages under {}", pages.mount));
    }
    if !manifest.server_events.is_empty() {
        adds.push("told what happens on the server".to_owned());
    }
    adds.extend(manifest.server_commands.iter().map(|command| format!("kito-server {}", command.name)));
    adds
}

/// What a plugin adds that a person sees at once: its pages, its way to
/// sign in. The running server starts and stops both as soon as the
/// plugin is enabled or disabled.
fn seen(manifest: &plugins::Manifest) -> Option<&'static str> {
    match (manifest.pages.is_some(), manifest.sign_in.is_some()) {
        (true, true) => Some("Its pages and its way to sign in"),
        (true, false) => Some("Its pages"),
        (false, true) => Some("Its way to sign in"),
        (false, false) => None,
    }
}

/// Every plugin the server's user has installed, with what the server
/// says of it, and what the registry lists.
pub fn listed(server: &Server) -> Value {
    let mut names: Vec<String> =
        plugins::all_installed().into_iter().map(|found| found.manifest.name).collect();
    names.extend(server.config.plugins.keys().cloned());
    names.sort();
    names.dedup();
    let installed: Vec<Value> = names
        .iter()
        .map(|name| {
            let found = plugins::installed(name);
            let config = server.config.plugins.get(name);
            let manifest = found.as_ref().map(|found| &found.manifest);
            json!({
                "name": name,
                "installed": found.is_some(),
                "version": manifest.map(|manifest| manifest.version.as_str()),
                "description": manifest.map(|manifest| manifest.description.as_str()),
                "runs": manifest.map(|manifest| manifest.runs()).unwrap_or_default(),
                "asks": manifest.map(|manifest| manifest.permissions.clone()).unwrap_or_default(),
                "adds": manifest.map(adds).unwrap_or_default(),
                "enabled": config.is_some_and(|config| config.enabled),
                "permissions": config.map(|config| config.permissions.clone()).unwrap_or_default(),
                "for_clients": config.is_some_and(|config| config.for_clients),
                // The one that serves this interface is not taken out from it.
                "serves_pages": manifest.is_some_and(|manifest| manifest.pages.is_some())
                    && config.is_some_and(|config| config.enabled),
                "problem": manifest.and_then(|manifest| manifest.unfit()),
                "last_failure": server.last_failure(name),
            })
        })
        .collect();
    let registry = kitod::registry::base();
    let mut said =
        json!({ "installed": installed, "registry": registry, "platform": kitod::registry::platform() });
    // Whether what the registry lists is checked against its key (ADR 0009).
    said["registry_signed"] = kitod::registry::checked().unwrap_or(false).into();
    match kitod::registry::index() {
        Ok(index) => {
            let all: Vec<Value> = index
                .plugins
                .iter()
                .map(|(name, listed)| {
                    let mut one = json!({ "name": name, "about": listed.about, "latest": listed.latest });
                    if !listed.runs.is_empty() {
                        one["runs"] = json!(listed.runs);
                    }
                    one
                })
                .collect();
            said["listed"] = all.into();
        }
        Err(why) => said["registry_error"] = why.into(),
    }
    said
}

/// Does what an administrator asked, and says what was done. `Err`: it
/// was not done, and why.
pub fn change(server: &mut Server, by: &str, ask: &Ask) -> Result<Value, String> {
    let name = ask.name.as_str();
    plugins::check_name("plugin", name)?;
    let serves = |server: &Server| {
        let enabled = server.config.plugins.get(name).is_some_and(|config| config.enabled);
        enabled && plugins::installed(name).is_some_and(|found| found.manifest.pages.is_some())
    };
    let told = |server: &Server, kind: &str, what: String| {
        server.tell(kind, json!({ "plugin": name, "by": by, "what": format!("{what}, by {by}") }));
    };
    match ask.action.as_str() {
        "install" => {
            if let Some(why) = kitod::registry::not_listed_for(name, "server") {
                return Err(why);
            }
            let (added, source) = kitod::registry::install(name, ask.version.as_deref())?;
            let manifest = &added.manifest;
            told(server, "plugin_installed", format!("{name} {}", manifest.version));
            Ok(json!({
                "done": format!("{name} {} is installed. It runs nowhere until it is enabled.", manifest.version),
                "from": source,
                "asks": manifest.permissions,
                "adds": adds(manifest),
            }))
        }
        "enable" => {
            let found = server.enable_plugin(name, ask.permissions.clone(), ask.for_clients)?;
            let given: Vec<&str> = server.config.plugins[name].permissions.iter().map(|p| p.word()).collect();
            let given = if given.is_empty() { "nothing".to_owned() } else { given.join(", ") };
            told(server, "plugin_enabled", format!("{name} ({given})"));
            let mut done = format!("{name} is enabled on this server. It may: {given}.");
            if let Some(what) = seen(&found.manifest) {
                done.push_str(&format!(" {what}: there now."));
            }
            Ok(json!({ "done": done }))
        }
        "disable" => {
            if serves(server) {
                return Err(format!(
                    "`{name}` serves the pages of this server, this one too: it is disabled on the server's \
                     machine (`kito-server plugin disable {name}`)"
                ));
            }
            let found = plugins::installed(name);
            let enabled = server.config.plugins.get(name).is_some_and(|config| config.enabled);
            // One last time, for what it set up on this server.
            let mut said = Vec::new();
            if let (true, Some(found)) = (enabled, &found) {
                let envs = [(ENV_SERVER_DIR, server.dir.as_os_str().to_owned())];
                match plugins::cleanup(found, "server", &server.dir, &envs) {
                    Some(Ok(printed)) => said.extend(printed.lines().map(str::to_owned)),
                    Some(Err(why)) => {
                        said.push(format!("it did not take out what it wrote ({why}): it goes all the same"))
                    }
                    None => {}
                }
            }
            if !server.disable_plugin(name, ask.purge)? {
                return Err(format!("`{name}` was not enabled on this server"));
            }
            told(server, "plugin_disabled", name.to_owned());
            let mut done = format!("{name} is disabled on this server.");
            if let Some(what) = found.as_ref().and_then(|found| seen(&found.manifest)) {
                done.push_str(&format!(" {what}: gone now."));
            }
            Ok(json!({ "done": done, "said": said }))
        }
        "remove" => {
            // Disabled first: what runs is not pulled from under a server.
            server.forget_plugin(name)?;
            let found =
                plugins::installed(name).ok_or_else(|| format!("the plugin `{name}` is not installed"))?;
            let mut said = Vec::new();
            match plugins::cleanup(&found, "person", &found.dir, &[]) {
                Some(Ok(printed)) => said.extend(printed.lines().map(str::to_owned)),
                Some(Err(why)) => {
                    said.push(format!("it did not take out what it wrote ({why}): it goes all the same"))
                }
                None => {}
            }
            plugins::uninstall(name)?;
            told(server, "plugin_removed", name.to_owned());
            Ok(json!({ "done": format!("{name} is removed from the server's machine."), "said": said }))
        }
        other => Err(format!("`{other}`? A plugin is installed, enabled, disabled or removed")),
    }
}
