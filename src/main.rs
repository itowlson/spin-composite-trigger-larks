use std::{path::{PathBuf, Path}, collections::{HashSet}};

use anyhow::{anyhow, Error, Result, Context};
use clap::{Parser};
use spin_app::{Loader, locked::{LockedApp, LockedTrigger}};
use url::Url;

#[derive(Debug, clap::Parser)]
#[clap(
    allow_hyphen_values = true,
)]
struct CompositeApp {
    args: Vec<std::ffi::OsString>,
}

#[tokio::main]
async fn main() -> Result<(), Error> {
    let t = CompositeApp::parse();
    t.run().await
}

impl CompositeApp {
    async fn run(&self) -> anyhow::Result<()> {
        let ctrlc = tokio::spawn(async move {
            tokio::signal::ctrl_c().await.unwrap();
        });
        
        // We are going to need to read the file at $SPIN_LOCKED_URL,
        // transform it into a file per trigger, and pass that on to
        // `spin trigger ...` or `spin trigger-...`
        //
        // We also need to extract the common and per-trigger args
        // and pass them through.

        let lockfile_url = std::env::var("SPIN_LOCKED_URL")?;
        let working_dir = PathBuf::from(std::env::var("SPIN_WORKING_DIR")?);
        let loader = spin_trigger::loader::TriggerLoader::new(&working_dir, false);
        let app = loader.load_app(&lockfile_url).await?;

        let trigger_types = app
            .triggers
            .iter()
            .filter_map(|t| t.trigger_config.get("type").and_then(|v| v.as_str().map(|vv| vv.to_owned())))
            .collect::<HashSet<_>>();

        let mut triggers = tokio::task::JoinSet::new();

        for trigger_type in trigger_types {
            let subapp = locked_app_for_trigger(&app, &trigger_type);
            let args = self.args_for_trigger(&trigger_type);
            let lockfile2 = write_locked_app(&subapp, &trigger_type, &working_dir).await?;
            let trigger_subcommand = if trigger_type == "http" || trigger_type == "redis" {
                vec!["trigger".to_owned(), trigger_type.to_owned()]
            } else {
                vec![format!("trigger-{trigger_type}")]
            };
            triggers.spawn(async move {
                let mut child = tokio::process::Command::new("spin")
                    .args(trigger_subcommand)
                    .args(args)
                    .env("SPIN_LOCKED_URL", &lockfile2)
                    .spawn()
                    .unwrap();
                child.wait().await
            });
        }

        tokio::select! {
            _ = ctrlc => {
                triggers.abort_all()
            },
            _ = triggers.join_next() => {
                triggers.abort_all()
            }
        };

        Ok(())
    }

    fn args_for_trigger(&self, trigger_type: &str) -> Vec<std::ffi::OsString> {
        let grupps = self.gruppified_args();
        let tt_prefix = format!("--{trigger_type}-");

        let mut args = vec![];

        for mut grupp in grupps {
            let opt_text = grupp[0].to_string_lossy();
            if opt_text.starts_with(&tt_prefix) {
                let new_opt_text = opt_text.replace(&tt_prefix, "--");
                grupp[0] = new_opt_text.into();
                args.extend(grupp.into_iter());
            }
            // otherwise we skip this grupp
            // TODO: friendlier handling for common args like --quiet - currently these would need to be e.g. --http-quiet --sqs-quiet
        }

        args
    }

    fn gruppified_args(&self) -> Vec<Vec<std::ffi::OsString>> {
        // they're not groups but I have no idea what they are
        let mut grupps = vec![];

        for arg in &self.args {
            if arg.to_string_lossy().starts_with('-') {
                // We are beginning a new grupp
                grupps.push(vec![]);
            }
            // TODO: this will do terrible things if the first arg is not hyphened
            grupps.last_mut().unwrap().push(arg.clone());
        }

        grupps
    }
}

fn locked_app_for_trigger(app: &LockedApp, trigger_type: &str) -> LockedApp {
    let mut subset = app.clone();

    // Restrict set of trigger-components
    subset.triggers = subset
        .triggers
        .into_iter()
        .filter(|t| t.trigger_config.get("type").and_then(|v| v.as_str()) == Some(trigger_type))
        .map(|t| uncompositify(t))
        .collect();

    // Restrict list of components
    subset.components = subset
        .components
        .into_iter()
        .filter(|c| is_id_present(&subset.triggers, &c.id))
        .collect();

    // Fix up metadata
    let trigger = subset.metadata.get("trigger").unwrap().clone();
    let o = trigger.as_object().unwrap();
    let mut o2 = serde_json::Map::new();
    let tt_prefix = format!("{trigger_type}-");
    for (k, v) in o.iter() {
        if k.starts_with(&tt_prefix) {
            o2.insert(k.replace(&tt_prefix, ""), v.clone());
        }
    }
    o2.insert("type".to_owned(), serde_json::Value::String(trigger_type.to_owned()));
    subset.metadata.insert("trigger".to_owned(), serde_json::Value::Object(o2));

    subset
}

fn is_id_present(triggers: &[LockedTrigger], id: &str) -> bool {
    triggers.iter().any(|t| trigger_component_id(t) == id)
}

fn trigger_component_id(trigger: &LockedTrigger) -> &str {
    let map = trigger.trigger_config.as_object().unwrap();
    map.get("component").and_then(|v| v.as_str()).unwrap()
}

fn uncompositify(mut t: LockedTrigger) -> LockedTrigger {
    let real_type = t.trigger_config.get("type").and_then(|v| v.as_str()).unwrap().to_owned();
    t.trigger_type = real_type;
    t.trigger_config.as_object_mut().unwrap().remove("type");
    t
}

// Duplicated from `up`
async fn write_locked_app(
    locked_app: &LockedApp,
    trigger_type: &str,
    working_dir: &Path,
) -> Result<String, anyhow::Error> {
    let locked_path = working_dir.join(format!("spin.{trigger_type}.lock"));
    let locked_app_contents =
        serde_json::to_vec_pretty(&locked_app).context("failed to serialize locked app")?;
    tokio::fs::write(&locked_path, locked_app_contents)
        .await
        .with_context(|| format!("failed to write {:?}", locked_path))?;
    let locked_url = Url::from_file_path(&locked_path)
        .map_err(|_| anyhow!("cannot convert to file URL: {locked_path:?}"))?
        .to_string();

    Ok(locked_url)
}
