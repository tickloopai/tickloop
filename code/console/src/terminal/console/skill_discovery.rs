use super::*;
use tickloop_code_settings::skills::{SkillDiscoveryCache, SkillRegistry};

const REFRESH_INTERVAL: Duration = Duration::from_millis(500);

impl Console {
    pub(in crate::terminal) fn configure_skill_completion(
        &self,
        root: Option<PathBuf>,
    ) -> Result<(), String> {
        let state = Arc::downgrade(&self.state);
        let stop = self._lifetime.stop.clone();
        thread::Builder::new()
            .name("skill-completion".into())
            .spawn(move || {
                let mut cache = SkillDiscoveryCache::default();
                while !stop.load(Ordering::Acquire) {
                    let Some(shared) = state.upgrade() else {
                        return;
                    };
                    let folder = match shared.lock() {
                        Ok(state) => state.working_directory().to_path_buf(),
                        Err(_) => return,
                    };
                    if let Some(registry) = cache.refresh(&folder, root.as_deref())
                        && let Ok(mut state) = shared.lock()
                        && state.working_directory() == folder
                    {
                        state.completion_skills = Some((folder, registry));
                        state.refresh_completion();
                        if !state.terminal_lent {
                            let _ = state.draw(&mut io::stdout().lock());
                        }
                    }
                    drop(shared);
                    thread::sleep(REFRESH_INTERVAL);
                }
            })
            .map_err(|error| error.to_string())?;
        Ok(())
    }
}

impl State {
    pub(super) fn refresh_skill_candidates(&mut self) {
        let registry = self
            .completion_skills
            .as_ref()
            .filter(|(folder, _)| folder == self.working_directory())
            .map(|(_, registry)| registry);
        self.completion
            .set_skills(registry.unwrap_or(&SkillRegistry::builtin()));
    }
}
