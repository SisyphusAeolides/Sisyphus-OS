#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ServiceState {
    Stopped,
    Starting,
    Running,
    Stopping,
    Failed,
}

pub struct Service<'a> {
    pub name: &'a str,
    pub executable: &'a str,
    pub state: ServiceState,
}
