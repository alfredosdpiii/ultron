//! The models a frame may name (`rlm.infer(model=...)`, `rlm.frameModel`, `ULTRON_RLM_FRAME_MODEL`): TypeScript
//! sets the frame task lane's model and the lane resolves it from the session's model registry with the provider's
//! own key (`getModel`, `getAuth`). Here the built-in catalog or models.json gives the model, ready for requests,
//! and the key is the one a lane of that provider would carry.

use crate::RunEnv as Env;
use crate::compose::Config;
use std::path::PathBuf;
use std::sync::Arc;

/// Resolves `provider/model` for frames: the catalog's (or models.json's) model and its provider's request key.
pub fn resolver(config: Arc<Config>) -> inference::ModelResolver {
    Arc::new(move |spec: &str| {
        let (provider, id) = spec
            .split_once('/')
            .ok_or_else(|| format!("model must be provider/model, not {spec}"))?;
        let model = config
            .model(provider, id)
            .ok_or_else(|| format!("Model \"{spec}\" not found"))?;
        let env = Env {
            cwd: config.cwd.clone(),
            home: config.home.clone(),
            var: config.var.clone(),
            docs: PathBuf::new(),
        };
        let key = crate::lane::api_key(&crate::ModelFlags::default(), &env, &model.provider);
        Ok((model, key))
    })
}
