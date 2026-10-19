//! `diagram.mermaid` for a session ([`kernel_apis::diagram`]): node from the session environment's `PATH`, Pele
//! unpacked under `<agent dir>/cache`, diagrams saved to `<agent dir>/rlm-output/<session id>/diagrams`, the theme
//! from the `theme` setting (read at each call) and the session's `COLORFGBG`, and shown diagrams handed to the
//! lane's cell images ([`crate::view_images::ViewImages`]): attached for the model with `to_model`, else kept for
//! the tool result's display only.

use crate::compose::Config;
use crate::view_images::ViewImages;
use kernel_apis::diagram::{DiagramConfig, DiagramModule, NODE_TIMEOUT, ShownDiagram, ThemeInputs};
use serde_json::{Map, Value, json};
use std::sync::Arc;

/// The diagram module of session `session_id`, showing diagrams through `images`.
pub fn module(config: &Config, session_id: &str, images: Arc<ViewImages>) -> DiagramModule {
    let (cwd, agent_dir, trusted, var) = (
        config.cwd.clone(),
        config.agent_dir.clone(),
        config.project_trusted,
        config.var.clone(),
    );
    DiagramModule::new(DiagramConfig {
        path_var: (config.var)("PATH"),
        home: config.home.clone(),
        cache_dir: config.agent_dir.join("cache"),
        out_dir: config.agent_dir.join("rlm-output").join(session_id).join("diagrams"),
        cwd: config.cwd.clone(),
        theme: Arc::new(move || {
            let settings = config::settings::SettingsManager::create(&cwd, &agent_dir, trusted);
            ThemeInputs {
                setting: settings
                    .settings()
                    .get("theme")
                    .and_then(Value::as_str)
                    .map(str::to_string),
                colorfgbg: var("COLORFGBG"),
                custom_dir: agent_dir.join("themes"),
            }
        }),
        show: Arc::new(move |shown: &ShownDiagram<'_>| {
            if shown.to_model {
                let mut payload = Map::new();
                payload.insert("path".into(), json!(shown.png_path.display().to_string()));
                images.attach(shown.lane, &payload).map(|_| ())
            } else {
                images.display(shown.lane, shown.png.to_vec(), shown.line.clone());
                Ok(())
            }
        }),
        fonts: None,
        timeout: NODE_TIMEOUT,
    })
}
