use crate::{config::Config, scheduler_client::SchedulerClient};
use coolify_storage::Store;

#[derive(Clone)]
pub struct AppState {
    pub store: Store,
    pub config: Config,
    pub scheduler: SchedulerClient,
}
impl AppState {
    pub fn new(store: Store, config: Config) -> Self {
        let scheduler = SchedulerClient::new(
            config.scheduler_socket_path.clone(),
            config.scheduler_timeout,
        );
        Self {
            store,
            config,
            scheduler,
        }
    }
}
